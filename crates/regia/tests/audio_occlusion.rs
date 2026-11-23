// SPDX-License-Identifier: MPL-2.0
//! Physics occlusion of entity emitters, end to end: a wall collider between the listener and an
//! emitter muffles it; the emitter's own body (its parent vehicle) does not; moving the wall
//! away clears it again.

use std::time::Duration;

use regia::audio_occlusion::{OcclusionRayStats, PhysicsAudioOcclusionPlugin};
use regia_app::{App, FixedTime, TimeUpdateStrategy};
use regia_audio::{
    Audio, AudioEmitter, AudioListener, AudioOcclusion, AudioPlugin, AudioSettings, OcclusionTuning,
    SoundAsset, SoundAssets, SoundClip,
};
use regia_core::math::Vec3;
use regia_ecs::prelude::*;
use regia_physics::{Collider, ColliderShape, PhysicsPlugin, RigidBody};
use regia_scene::{ScenePlugin, Transform};

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
    app.add_plugins((
        ScenePlugin,
        PhysicsPlugin,
        AudioPlugin,
        PhysicsAudioOcclusionPlugin,
    ));
    let clip = SoundClip::new(vec![0.1; 4_800], 48_000).expect("clip");
    app.world_mut()
        .resource_mut::<SoundAssets>()
        .add("hum", SoundAsset::from_clip(&clip));
    app
}

fn wall(app: &mut App, at: Vec3) -> Entity {
    app.world_mut()
        .spawn((
            Transform::from_translation(at),
            RigidBody::static_body(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::new(3.0, 3.0, 0.2),
                },
                ..Collider::default()
            },
        ))
        .id()
}

fn occlusion_of(app: &mut App, emitter: Entity, frames: usize) -> f32 {
    for _ in 0..frames {
        app.update();
        let _ = app.world_mut().resource_mut::<Audio>().render_offline(800);
    }
    app.world().resource::<AudioOcclusion>().reported(emitter)
}

#[test]
fn a_wall_between_listener_and_emitter_occludes_it() {
    let mut app = app();
    app.world_mut()
        .spawn((AudioListener::default(), Transform::default()));
    // The emitter rides on a vehicle whose own collider surrounds it: that must not occlude.
    let vehicle = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::NEG_Z * 10.0),
            RigidBody::static_body(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::splat(1.5),
                },
                ..Collider::default()
            },
        ))
        .id();
    let emitter = app
        .world_mut()
        .spawn((
            AudioEmitter {
                sound_name: "hum".into(),
                ..AudioEmitter::default()
            },
            Transform::default(),
            ChildOf(vehicle),
        ))
        .id();
    assert_eq!(
        occlusion_of(&mut app, emitter, 10),
        0.0,
        "own vehicle does not occlude"
    );

    let blocker = wall(&mut app, Vec3::NEG_Z * 5.0);
    let amount = occlusion_of(&mut app, emitter, 10);
    assert!(amount > 0.5, "a wall occludes: {amount}");
    let status = app.world().resource::<Audio>().emitter_status();
    assert!(
        status[0].occlusion > 0.3,
        "the emitter smooths toward it: {}",
        status[0].occlusion
    );

    // Two walls occlude more than one.
    wall(&mut app, Vec3::NEG_Z * 3.0);
    let two = occlusion_of(&mut app, emitter, 10);
    assert!(two > amount, "{two} vs {amount}");

    // Clear line of sight again.
    app.world_mut().despawn(blocker);
    let world = app.world_mut();
    let mut walls = world.query_filtered::<Entity, (With<Collider>, Without<ChildOf>)>();
    let others: Vec<Entity> = walls.iter(world).filter(|e| *e != vehicle).collect();
    for e in others {
        app.world_mut().despawn(e);
    }
    assert_eq!(occlusion_of(&mut app, emitter, 10), 0.0, "walls gone");
}

/// Sixty-four separate boxes on one segment saturate the occlusion amount at 16 obstacles,
/// and the cast examines only those 16.
#[test]
fn a_dense_segment_saturates_at_sixteen_obstacles() {
    let mut app = app();
    app.world_mut().resource_mut::<AudioOcclusion>().rays_per_frame = 1;
    app.world_mut()
        .spawn((AudioListener::default(), Transform::default()));
    let emitter = app
        .world_mut()
        .spawn((
            AudioEmitter {
                sound_name: "hum".into(),
                max_distance: 80.0,
                ..AudioEmitter::default()
            },
            Transform::from_translation(Vec3::new(47.0, 0.0, 0.0)),
        ))
        .id();
    for i in 1..=64 {
        wall_between(&mut app, Vec3::new(i as f32 * 0.7, 0.0, 0.0));
    }
    let amount = occlusion_of(&mut app, emitter, 12);
    let examined = app.world().resource::<OcclusionRayStats>().last_ray_entities;
    let saturated = OcclusionTuning::default().amount_for_obstacles(16);
    eprintln!("dense occlusion: examined={examined} amount={amount} saturated={saturated}");
    assert!(
        (amount - saturated).abs() < 1e-5,
        "64 solids saturate at 16: amount {amount} saturated {saturated}"
    );
    assert_eq!(
        examined, 16,
        "a saturated ray examined {examined} solid entities, the cap is 16"
    );
}

fn wall_between(app: &mut App, at: Vec3) {
    app.world_mut().spawn((
        Transform::from_translation(at),
        RigidBody::static_body(),
        Collider {
            shape: ColliderShape::Box {
                half_extents: Vec3::new(0.15, 2.0, 2.0),
            },
            ..Collider::default()
        },
    ));
}
