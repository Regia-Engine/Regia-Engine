// SPDX-License-Identifier: MPL-2.0
//! Contact sounds from physics (ADR-3270): the acceptance scenes of the physics-contact-audio lane.
//!
//! Fixed steps and fixed inputs only: every scene advances one 60 Hz fixed step per update and
//! counts steps, never wall time. The sounds are placeholders generated here (a decaying click and
//! two tones), not recordings.

#![cfg(feature = "client")]

use std::time::Duration;

use regia_app::{App, FixedTime, TimeUpdateStrategy};
use regia_audio::{
    AudioPlugin, AudioSettings, ContactLoop, ImpactBank, ImpactBankDef, ImpactSet, SoundAsset, SoundAssets,
    SoundClip,
};
use regia_core::math::Vec3;
use regia_ecs::prelude::{ChildOf, Entity};
use regia_framework::physics_audio::{PhysicsAudioPlugin, PhysicsAudioState, PhysicsFootstepSurfaces};
use regia_physics::destruction::{DestructibleId, DestructionFx, DestructionFxMessage, FxKind};
use regia_physics::{
    ActiveEvents, Collider, ColliderShape, PhysicsMaterial, PhysicsMaterialRef, PhysicsMaterials,
    PhysicsPlugin, RigidBody, Velocity,
};
use regia_scene::{GlobalTransform, Transform};

/// A placeholder one-shot: a 40 ms decaying click (generated, not recorded).
fn click() -> SoundAsset {
    let samples: Vec<f32> = (0..1920)
        .map(|i| {
            let t = i as f32 / 48_000.0;
            (t * 2_400.0 * std::f32::consts::TAU).sin() * (-t * 80.0).exp() * 0.5
        })
        .collect();
    SoundAsset::from_clip(&SoundClip::new(samples, 48_000).expect("a clip"))
}

/// A placeholder loop: 100 ms of a tone (generated, not recorded).
fn tone(hz: f32) -> SoundAsset {
    let samples: Vec<f32> = (0..4800)
        .map(|i| (i as f32 / 48_000.0 * hz * std::f32::consts::TAU).sin() * 0.2)
        .collect();
    SoundAsset::from_clip(&SoundClip::new(samples, 48_000).expect("a clip"))
}

fn bank_def() -> ImpactBankDef {
    let mut def = ImpactBankDef::default();
    let names = |p: &str| (0..4).map(|i| format!("{p}_{i}")).collect::<Vec<_>>();
    let quiet = |set: ImpactSet| ImpactSet {
        pitch_jitter: 0.0,
        volume_jitter_db: 0.0,
        ..set
    };
    def.materials.insert(
        "wood".into(),
        quiet(ImpactSet {
            scrape: Some("wood_scrape".into()),
            roll: Some("wood_roll".into()),
            ..ImpactSet::new(names("wood"))
        }),
    );
    def.materials.insert(
        "concrete".into(),
        quiet(ImpactSet {
            scrape: Some("concrete_scrape".into()),
            roll: Some("concrete_roll".into()),
            ..ImpactSet::new(names("concrete"))
        }),
    );
    def.materials
        .insert("default".into(), quiet(ImpactSet::new(names("thud"))));
    def.destruction_materials
        .insert("brick".into(), "concrete".into());
    def.validate().expect("a valid bank");
    def
}

/// Physics + headless audio + the contact-sound plugin at 60 Hz, a concrete floor whose top is at
/// y = 0, and the placeholder sounds.
fn app() -> App {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    let mut fixed = FixedTime::default();
    fixed.hz = 60.0;
    app.insert_resource(fixed);
    app.insert_resource(AudioSettings {
        headless: true,
        ..Default::default()
    });
    app.add_plugins((PhysicsPlugin, AudioPlugin, PhysicsAudioPlugin));
    let mut sounds = SoundAssets::default();
    for prefix in ["wood", "concrete", "thud"] {
        for i in 0..4 {
            sounds.add(format!("{prefix}_{i}"), click());
        }
    }
    for (name, hz) in [
        ("wood_scrape", 900.0),
        ("wood_roll", 120.0),
        ("concrete_scrape", 1_400.0),
        ("concrete_roll", 90.0),
    ] {
        sounds.add(name, tone(hz));
    }
    app.insert_resource(sounds);
    app.insert_resource(ImpactBank::new(bank_def()));
    let mut materials = PhysicsMaterials::default();
    for (name, friction) in [("wood", 0.4), ("concrete", 0.6)] {
        let mut m = PhysicsMaterial::new(name);
        m.friction = friction;
        materials.insert(m);
    }
    app.insert_resource(materials);
    app.world_mut().spawn((
        Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
        RigidBody::static_body(),
        Collider {
            shape: ColliderShape::Box {
                half_extents: Vec3::new(60.0, 0.5, 60.0),
            },
            active_events: ActiveEvents::ALL,
            ..Collider::default()
        },
        PhysicsMaterialRef("concrete".into()),
    ));
    app
}

fn sounding_collider(shape: ColliderShape) -> Collider {
    Collider {
        shape,
        active_events: ActiveEvents {
            collision: false,
            contact_force: true,
        },
        contact_force_threshold: 0.0,
        ..Collider::default()
    }
}

/// A 0.5 m wooden box whose bottom is `height` above the floor.
fn spawn_box(app: &mut App, x: f32, z: f32, height: f32, velocity: Vec3) -> Entity {
    app.world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(x, 0.25 + height, z)),
            RigidBody {
                can_sleep: false,
                ..RigidBody::dynamic()
            },
            sounding_collider(ColliderShape::Box {
                half_extents: Vec3::splat(0.25),
            }),
            Velocity {
                linear: velocity,
                angular: Vec3::ZERO,
            },
            PhysicsMaterialRef("wood".into()),
        ))
        .id()
}

/// Runs updates until the contact-sound system has done `steps` more fixed steps.
fn step(app: &mut App, steps: u64) {
    let target = app.world().resource::<PhysicsAudioState>().step() + steps;
    let mut guard = 0;
    while app.world().resource::<PhysicsAudioState>().step() < target {
        app.update();
        guard += 1;
        assert!(guard < steps * 4 + 16, "the fixed step does not advance");
    }
}

fn impacts_of(app: &App, body: Entity) -> Vec<regia_audio::ImpactPlay> {
    app.world()
        .resource::<ImpactBank>()
        .recent()
        .filter(|p| p.kind == "impact" && p.body == body.to_bits())
        .cloned()
        .collect()
}

#[test]
fn drops_from_three_heights_give_one_impact_each_louder_with_height() {
    let mut app = app();
    let heights = [0.5_f32, 2.0, 8.0];
    let boxes: Vec<Entity> = heights
        .iter()
        .enumerate()
        .map(|(i, &h)| spawn_box(&mut app, i as f32 * 4.0, 0.0, h, Vec3::ZERO))
        .collect();
    step(&mut app, 300);
    let mut last_db = f32::NEG_INFINITY;
    for (&h, &body) in heights.iter().zip(&boxes) {
        let plays = impacts_of(&app, body);
        assert_eq!(plays.len(), 1, "a drop from {h} m: {plays:?}");
        let play = &plays[0];
        assert_eq!(play.materials.0, "wood", "the falling box's material comes first");
        assert!(play.started, "the placeholder variant played");
        assert!(
            play.volume_db > last_db,
            "{h} m is not louder than the lower drop: {} <= {last_db}",
            play.volume_db
        );
        last_db = play.volume_db;
        let expected = (2.0 * 9.81 * h).sqrt();
        assert!(
            (play.normal_speed_m_s - expected).abs() < 0.15 * expected + 0.2,
            "approach speed {} vs free fall {expected}",
            play.normal_speed_m_s
        );
    }
}

#[test]
fn a_resting_box_makes_no_new_impacts() {
    let mut app = app();
    let body = spawn_box(&mut app, 0.0, 0.0, 0.3, Vec3::ZERO);
    step(&mut app, 120);
    let settled = impacts_of(&app, body).len();
    assert_eq!(settled, 1, "the short drop sounds once");
    let before = app.world().resource::<PhysicsAudioState>().stats;
    step(&mut app, 600);
    let after = app.world().resource::<PhysicsAudioState>().stats;
    assert_eq!(impacts_of(&app, body).len(), settled, "no jitter retriggers");
    assert!(
        after.contacts >= before.contacts + 600,
        "the resting contact kept reporting a force"
    );
    assert_eq!(after.impacts, before.impacts);
}

#[test]
fn a_sliding_box_scrapes_then_fades_to_silence() {
    let mut app = app();
    let body = spawn_box(&mut app, 0.0, 0.0, 0.0, Vec3::new(4.0, 0.0, 0.0));
    let floor = floor_entity(&mut app);
    let fade = u64::from(app.world().resource::<ImpactBank>().budget().fade_steps);
    let mut slid = 0;
    let mut stopped_at = None;
    for n in 0..240u64 {
        step(&mut app, 1);
        let speed = app
            .world()
            .get::<Velocity>(body)
            .expect("velocity")
            .linear
            .length();
        let state = app.world().resource::<PhysicsAudioState>();
        let pair = state.pair(body, floor);
        if speed > 0.3 && n > 2 {
            let pair = pair.expect("a sliding pair");
            assert_eq!(pair.kind, ContactLoop::Scrape, "step {n}");
            assert!(pair.gain > 0.0, "step {n}: speed {speed}");
            assert!(pair.voice.is_some(), "step {n}: the scrape loop plays");
            slid += 1;
        }
        if speed < 0.01 && stopped_at.is_none() && slid > 0 {
            stopped_at = Some(n);
        }
        if let Some(at) = stopped_at
            && n > at + fade
        {
            assert!(pair.is_none_or(|p| p.gain == 0.0), "step {n}: still {pair:?}");
        }
    }
    assert!(slid > 20, "the box slid for {slid} steps");
    assert!(stopped_at.is_some(), "the box stopped");
    let state = app.world().resource::<PhysicsAudioState>();
    assert!(state.stats.loops_started >= 1 && state.stats.loops_stopped >= 1);
}

#[test]
fn a_rolling_sphere_rolls_and_never_scrapes() {
    let mut app = app();
    let radius = 0.25;
    let speed = 3.0;
    let ball = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, radius, 0.0)),
            RigidBody {
                can_sleep: false,
                ..RigidBody::dynamic()
            },
            sounding_collider(ColliderShape::Sphere { radius }),
            Velocity {
                linear: Vec3::new(speed, 0.0, 0.0),
                // Rolling without slipping along +x: ω × (0, −r, 0) = (−v, 0, 0).
                angular: Vec3::new(0.0, 0.0, -speed / radius),
            },
            PhysicsMaterialRef("wood".into()),
        ))
        .id();
    let floor = floor_entity(&mut app);
    let mut rolled = 0;
    for n in 0..120 {
        step(&mut app, 1);
        let state = app.world().resource::<PhysicsAudioState>();
        if let Some(pair) = state.pair(ball, floor) {
            assert_eq!(pair.kind, ContactLoop::Roll, "step {n}: {pair:?}");
            if pair.gain > 0.0 {
                rolled += 1;
            }
        }
    }
    assert!(rolled > 100, "rolled {rolled} of 120 steps");
    assert!(impacts_of(&app, ball).is_empty(), "rolling is not an impact");
}

#[test]
fn two_hundred_boxes_respect_the_cap_and_count_the_drops() {
    let mut app = app();
    for i in 0..20 {
        for k in 0..10 {
            spawn_box(
                &mut app,
                i as f32 * 1.0 - 10.0,
                k as f32 * 1.0 - 5.0,
                2.0,
                Vec3::ZERO,
            );
        }
    }
    let cap = app
        .world()
        .resource::<ImpactBank>()
        .budget()
        .max_new_impacts_per_step;
    step(&mut app, 120);
    let stats = app.world().resource::<PhysicsAudioState>().stats;
    assert!(stats.max_impacts_in_a_step <= cap, "{stats:?}");
    assert!(stats.impacts >= u64::from(cap), "{stats:?}");
    assert!(
        stats.dropped_by_budget >= 150,
        "the quietest were dropped and counted: {stats:?}"
    );
    assert!(
        stats.impacts + stats.dropped_by_budget >= 200,
        "every landing either played or was counted: {stats:?}"
    );
}

#[test]
fn the_same_seed_picks_the_same_variants() {
    let run = |seed: u64| {
        let mut app = app();
        app.world_mut().resource_mut::<ImpactBank>().reseed(seed);
        let boxes: Vec<Entity> = (0..6)
            .map(|i| spawn_box(&mut app, i as f32 * 3.0, 0.0, 0.5 + i as f32 * 0.4, Vec3::ZERO))
            .collect();
        step(&mut app, 200);
        let bank = app.world().resource::<ImpactBank>();
        let plays: Vec<(usize, usize, i32)> = bank
            .recent()
            .map(|p| {
                let index = boxes
                    .iter()
                    .position(|b| b.to_bits() == p.body)
                    .unwrap_or(usize::MAX);
                (index, p.variant, (p.volume_db * 1000.0) as i32)
            })
            .collect();
        plays
    };
    let a = run(77);
    assert_eq!(a.len(), 6, "{a:?}");
    assert_eq!(a, run(77), "the same seed replays the same sequence");
    assert_ne!(a, run(78), "another seed picks another sequence");
}

#[test]
fn the_footstep_probe_returns_the_mapped_surface() {
    let mut app = app();
    let mut surfaces = PhysicsFootstepSurfaces::default();
    surfaces.surfaces.insert("concrete".into(), "stone".into());
    app.insert_resource(surfaces);
    regia_framework::physics_audio::install_physics_footstep_probe(&mut app);
    app.add_plugins(regia_framework::footsteps::FootstepPlugin);
    let foot = Vec3::new(3.0, 0.0, 2.0);
    app.world_mut().spawn((
        regia_framework::footsteps::FootstepWalker::default(),
        Transform::from_translation(foot),
        GlobalTransform(Transform::from_translation(foot).compute_affine()),
    ));
    step(&mut app, 3);
    app.update();
    let probe = app
        .world()
        .resource::<regia_framework::footsteps::FootstepSurfaceProbe>();
    assert_eq!((probe.0)(foot), Some("stone".to_string()));
    assert_eq!(
        (probe.0)(foot + Vec3::X),
        None,
        "only the refreshed feet are known"
    );
    let s = app.world().resource::<PhysicsFootstepSurfaces>();
    assert_eq!(
        s.surface_for("wood"),
        "wood",
        "an unmapped material is its own surface"
    );
    // A host that replaces the mapping keeps the installed probe working.
    let mut remapped = PhysicsFootstepSurfaces::default();
    remapped.surfaces.insert("concrete".into(), "pavement".into());
    app.insert_resource(remapped);
    app.update();
    let probe = app
        .world()
        .resource::<regia_framework::footsteps::FootstepSurfaceProbe>();
    assert_eq!((probe.0)(foot), Some("pavement".to_string()));
}

#[test]
fn the_footstep_ray_skips_the_walkers_own_collider() {
    let mut app = app();
    let mut surfaces = PhysicsFootstepSurfaces::default();
    surfaces.surfaces.insert("concrete".into(), "stone".into());
    app.insert_resource(surfaces);
    regia_framework::physics_audio::install_physics_footstep_probe(&mut app);
    app.add_plugins(regia_framework::footsteps::FootstepPlugin);
    let foot = Vec3::new(3.0, 0.0, 2.0);
    // A character whose capsule `Collider` sits on the walker entity itself: the surface ray
    // starts 0.25 m above the foot, inside the capsule. It must skip the walker and reach the
    // concrete floor, or the footstep surface is lost.
    app.world_mut().spawn((
        regia_framework::footsteps::FootstepWalker::default(),
        Transform::from_translation(foot),
        GlobalTransform(Transform::from_translation(foot).compute_affine()),
        RigidBody::kinematic(),
        Collider {
            shape: ColliderShape::Capsule {
                half_height: 0.55,
                radius: 0.35,
                axis: regia_physics::ShapeAxis::Y,
            },
            offset: Transform::from_translation(Vec3::new(0.0, 0.9, 0.0)),
            ..Collider::default()
        },
    ));
    step(&mut app, 3);
    app.update();
    let probe = app
        .world()
        .resource::<regia_framework::footsteps::FootstepSurfaceProbe>();
    assert_eq!(
        (probe.0)(foot),
        Some("stone".to_string()),
        "the ray reaches the floor past the walker's capsule"
    );
}

#[test]
fn inaudible_impacts_charge_neither_the_cap_nor_the_stats() {
    let mut app = app();
    // Ten boxes land in the same fixed step ~45 m from the listener, beyond the emitter's
    // 40 m range: every play resolves a sound but starts no voice (`started: false`).
    for i in 0..10 {
        spawn_box(&mut app, 45.0, -9.0 + i as f32 * 2.0, 2.0, Vec3::ZERO);
    }
    step(&mut app, 120);
    let bank = app.world().resource::<ImpactBank>();
    assert_eq!(
        bank.played(),
        0,
        "no inaudible impact is counted as played: {:?}",
        bank.recent()
            .map(|p| (p.sound.clone(), p.started))
            .collect::<Vec<_>>()
    );
    assert!(
        bank.recent().all(|p| !p.started),
        "every logged play started no voice (out of range)"
    );
    let stats = &app.world().resource::<PhysicsAudioState>().stats;
    assert_eq!(stats.impacts, 0, "the step stats count only started voices");
    assert_eq!(
        stats.dropped_by_budget, 0,
        "the per-step cap is not consumed by plays that started nothing"
    );
}

#[test]
fn swapping_the_scrape_clip_mid_slide_restarts_the_voice() {
    let mut app = app();
    let body = spawn_box(&mut app, 0.0, 0.0, 0.0, Vec3::new(4.0, 0.0, 0.0));
    let floor = floor_entity(&mut app);
    step(&mut app, 6);
    {
        let state = app.world().resource::<PhysicsAudioState>();
        let pair = state.pair(body, floor).expect("a sliding pair");
        assert!(pair.voice.is_some(), "the scrape loop plays before the swap");
        assert_eq!(state.stats.loops_started, 1);
    }
    // The host swaps the bank's wood scrape clip while the box keeps sliding.
    app.world_mut()
        .resource_mut::<SoundAssets>()
        .add("wood_scrape_alt", tone(333.0));
    let mut def = bank_def();
    def.materials.get_mut("wood").expect("the wood set").scrape = Some("wood_scrape_alt".into());
    def.validate().expect("a valid bank");
    app.world_mut().resource_mut::<ImpactBank>().set_def(def);
    step(&mut app, 3);
    let state = app.world().resource::<PhysicsAudioState>();
    assert!(
        state.stats.loops_stopped >= 1 && state.stats.loops_started >= 2,
        "the changed clip restarts the voice: started {}, stopped {}",
        state.stats.loops_started,
        state.stats.loops_stopped
    );
    let pair = state.pair(body, floor).expect("the pair still slides");
    assert!(pair.voice.is_some(), "the new clip keeps playing");
}

#[test]
fn one_destruction_break_plays_one_break_sound() {
    let mut app = app();
    step(&mut app, 2);
    for (kind, magnitude) in [
        (FxKind::BondBreak, 40.0),
        (FxKind::BondBreak, 90.0),
        (FxKind::ChunkDetach, 12.0),
    ] {
        app.world_mut().write_message(DestructionFxMessage(DestructionFx {
            destructible: DestructibleId(3),
            kind,
            position: [1.0, 2.0, 3.0],
            magnitude,
            fx_tag: "brick".into(),
            seed: 5,
        }));
    }
    step(&mut app, 10);
    let state = app.world().resource::<PhysicsAudioState>();
    assert_eq!(state.stats.breaks, 1, "{:?}", state.stats);
    let bank = app.world().resource::<ImpactBank>();
    let breaks: Vec<_> = bank.recent().filter(|p| p.kind == "break").collect();
    assert_eq!(breaks.len(), 1);
    assert_eq!(breaks[0].materials, ("brick".to_string(), "concrete".to_string()));
    assert_eq!(breaks[0].impulse_ns, 90.0, "the strongest moment of the break");
    assert!(breaks[0].sound.starts_with("concrete_"));
}

fn floor_entity(app: &mut App) -> Entity {
    let mut query = app
        .world_mut()
        .query::<(Entity, &RigidBody, &PhysicsMaterialRef)>();
    query
        .iter(app.world())
        .find(|(_, body, m)| body.kind == regia_physics::BodyKind::Static && m.0 == "concrete")
        .map(|(e, _, _)| e)
        .expect("the floor")
}

#[test]
fn removing_the_bank_stops_every_loop() {
    let mut app = app();
    let ball = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, 0.25, 0.0)),
            RigidBody {
                can_sleep: false,
                ..RigidBody::dynamic()
            },
            sounding_collider(ColliderShape::Sphere { radius: 0.25 }),
            Velocity {
                linear: Vec3::new(2.0, 0.0, 0.0),
                angular: Vec3::new(0.0, 0.0, -8.0),
            },
            PhysicsMaterialRef("wood".into()),
        ))
        .id();
    let floor = floor_entity(&mut app);
    step(&mut app, 30);
    let state = app.world().resource::<PhysicsAudioState>();
    assert!(
        state.pair(ball, floor).is_some_and(|p| p.voice.is_some()),
        "the roll loop plays"
    );
    let stopped = state.stats.loops_stopped;
    app.world_mut().remove_resource::<ImpactBank>();
    for _ in 0..3 {
        app.update();
    }
    let state = app.world().resource::<PhysicsAudioState>();
    assert!(state.pairs().is_empty(), "no pair outlives the bank");
    assert_eq!(state.stats.loops_stopped, stopped + 1, "its voice was stopped");
}

/// Review rev-kimi-20 finding 6: the footstep ray skips the walker's whole body, not only a
/// collider on the walker entity itself: a capsule on a child of the walker (the usual
/// character-root layout) and a walker on a child "feet" entity of the capsule body.
#[test]
fn the_footstep_ray_skips_the_walkers_whole_body() {
    let mut app = app();
    let mut surfaces = PhysicsFootstepSurfaces::default();
    surfaces.surfaces.insert("concrete".into(), "stone".into());
    app.insert_resource(surfaces);
    regia_framework::physics_audio::install_physics_footstep_probe(&mut app);
    app.add_plugins(regia_framework::footsteps::FootstepPlugin);
    let capsule = || Collider {
        shape: ColliderShape::Capsule {
            half_height: 0.55,
            radius: 0.35,
            axis: regia_physics::ShapeAxis::Y,
        },
        offset: Transform::from_translation(Vec3::new(0.0, 0.9, 0.0)),
        ..Collider::default()
    };
    // (a) The walker is the body root; its capsule sits on a child entity.
    let foot_a = Vec3::new(3.0, 0.0, 2.0);
    let walker = app
        .world_mut()
        .spawn((
            regia_framework::footsteps::FootstepWalker::default(),
            Transform::from_translation(foot_a),
            GlobalTransform(Transform::from_translation(foot_a).compute_affine()),
            RigidBody::kinematic(),
        ))
        .id();
    app.world_mut()
        .spawn((Transform::IDENTITY, ChildOf(walker), capsule()));
    // (b) The capsule body is the root; the walker is a child "feet" entity.
    let foot_b = Vec3::new(-3.0, 0.0, 2.0);
    let body = app
        .world_mut()
        .spawn((
            Transform::from_translation(foot_b),
            RigidBody::kinematic(),
            capsule(),
        ))
        .id();
    app.world_mut().spawn((
        regia_framework::footsteps::FootstepWalker::default(),
        Transform::IDENTITY,
        ChildOf(body),
        GlobalTransform(Transform::from_translation(foot_b).compute_affine()),
    ));
    step(&mut app, 3);
    app.update();
    let probe = app
        .world()
        .resource::<regia_framework::footsteps::FootstepSurfaceProbe>();
    assert_eq!(
        (probe.0)(foot_a),
        Some("stone".to_string()),
        "the ray passes the capsule on the walker's child"
    );
    assert_eq!(
        (probe.0)(foot_b),
        Some("stone".to_string()),
        "the ray passes the capsule on the walker's parent body"
    );
}

/// Review rev-kimi-20 finding 5: an out-of-range rattle (a box hopping every 6 steps ~45 m from
/// the listener) is cooled down like an audible one, so it does not plan and log an impact on
/// every landing. Gaps are counted in fixed steps.
#[test]
fn an_inaudible_rattle_is_still_cooled_down() {
    let mut app = app();
    let body = spawn_box(&mut app, 45.0, 0.0, 0.0, Vec3::ZERO);
    step(&mut app, 30);
    let cooldown = u64::from(app.world().resource::<ImpactBank>().budget().cooldown_steps);
    let mut logged_at = Vec::new();
    let mut logged = impacts_of(&app, body).len();
    for n in 0..120u64 {
        if n % 6 == 0 {
            // A small hop: it lands within the 6 steps, at ~0.4 m/s (above the impact minimum).
            app.world_mut()
                .get_mut::<Velocity>(body)
                .expect("velocity")
                .linear = Vec3::new(0.0, 0.4, 0.0);
        }
        step(&mut app, 1);
        let now = impacts_of(&app, body).len();
        for _ in logged..now {
            logged_at.push(n);
        }
        logged = now;
    }
    let plays = impacts_of(&app, body);
    assert!(plays.iter().all(|p| !p.started), "out of range: {plays:?}");
    assert!(logged_at.len() >= 3, "the hops make impacts: {logged_at:?}");
    for pair in logged_at.windows(2) {
        assert!(
            pair[1] - pair[0] > cooldown,
            "an inaudible impact restarted within the {cooldown}-step cooldown: steps {logged_at:?}"
        );
    }
}
