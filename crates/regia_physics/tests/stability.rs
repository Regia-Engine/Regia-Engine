// SPDX-License-Identifier: MPL-2.0
//! Physics stability acceptance (sprint 2026-10-21, ADR-541; ported onto ADR-870's
//! `PhysicsSettings` solver fields at integration): continuous collision, sleeping
//! and waking, runtime body-kind changes, contact/trigger ends on removal, and joint
//! stability (long chains, mass ratios, limits, motors, cone limits).
//!
//! Every test steps a fixed 60 Hz world a fixed number of times with deterministic inputs;
//! no wall clock is read. Expectations are elementary mechanics (a body under a limit rests
//! on it, a hanging chain keeps its links together) with tolerances wide enough for a soft
//! iterative solver, and tight enough to fail when the behavior is missing (a tunnelled
//! bullet ends metres past the wall; an unlimited swing passes 1 rad).
//!
//! Provenance: the pipeline exercised is rapier3d (Apache-2.0); its CCD tiers (fast bodies
//! sweep fixed geometry, `ccd_enabled` bodies also sweep kinematic and dynamic bodies) and its
//! sleep rules are from rapier's public docs and source comments. The scenarios are Regia Engine's.

use std::time::Duration;

use regia_app::{App, FixedTime, TimeUpdateStrategy};
use regia_core::math::{Quat, Vec3};
use regia_ecs::ecs::message::{MessageCursor, Messages};
use regia_ecs::prelude::{ChildOf, Entity};
use regia_physics::{
    ActiveEvents, BodyKind, CharacterBody, Collider, ColliderShape, CollisionEnded, CollisionStarted,
    ExternalImpulse, Joint, JointKind, JointMotorConfig, JointNoContacts, PhysicsPlugin, PhysicsSettings,
    PhysicsWorld, RigidBody, RigidBodyHandle, Teleport, TriggerEnter, TriggerExit, Velocity,
};
use regia_scene::Transform;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// One fixed step per update: manual 1/60 s frames at a 60 Hz fixed rate. The frame and the
/// step are the same length, so the interpolation fraction is 0 and a dynamic body's
/// rendered `Transform` is the pose of the step before the last one.
fn physics_app() -> App {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    let mut fixed = FixedTime::default();
    fixed.hz = 60.0;
    app.insert_resource(fixed).add_plugins(PhysicsPlugin);
    app
}

/// Turns gravity off (the plugin inserted the default settings).
fn zero_gravity(app: &mut App) {
    app.insert_resource(PhysicsSettings {
        gravity: Vec3::ZERO,
        ..PhysicsSettings::default()
    });
}

fn steps(app: &mut App, count: usize) {
    for _ in 0..count {
        app.update();
    }
}

/// A static ground plate whose top surface is at y = 0.
fn ground(app: &mut App) -> Entity {
    app.world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
            RigidBody::static_body(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::new(10.0, 0.5, 10.0),
                },
                friction: 0.8,
                ..Collider::default()
            },
        ))
        .id()
}

/// A dynamic 1 m box (1 kg at density 1) at `translation`.
fn unit_box(app: &mut App, translation: Vec3) -> Entity {
    app.world_mut()
        .spawn((
            Transform::from_translation(translation),
            RigidBody::dynamic(),
            Collider {
                friction: 0.8,
                ..Collider::default()
            },
            Velocity::default(),
        ))
        .id()
}

fn translation_of(app: &App, entity: Entity) -> Vec3 {
    app.world()
        .get::<Transform>(entity)
        .expect("transform")
        .translation
}

fn rotation_of(app: &App, entity: Entity) -> Quat {
    app.world().get::<Transform>(entity).expect("transform").rotation
}

/// The rapier body behind an entity.
fn rapier_body(app: &App, entity: Entity) -> &rapier3d::dynamics::RigidBody {
    let handle = app
        .world()
        .get::<RigidBodyHandle>(entity)
        .expect("body handle")
        .to_handle();
    app.world()
        .resource::<PhysicsWorld>()
        .bodies
        .get(handle)
        .expect("rapier body")
}

fn is_sleeping(app: &App, entity: Entity) -> bool {
    rapier_body(app, entity).is_sleeping()
}

/// The world position of a local anchor on a root body.
fn world_anchor(app: &App, entity: Entity, anchor: Vec3) -> Vec3 {
    translation_of(app, entity) + rotation_of(app, entity) * anchor
}

/// Reads every new message of type `M` since the cursor.
fn read_new<M: regia_ecs::ecs::message::Message + Clone>(app: &App, cursor: &mut MessageCursor<M>) -> Vec<M> {
    cursor
        .read(app.world().resource::<Messages<M>>())
        .cloned()
        .collect()
}

// ---------------------------------------------------------------------------
// Continuous collision
// ---------------------------------------------------------------------------

/// Bullet speed: 5 m per 60 Hz step, under rapier's 400 m/s per-body velocity cap.
const BULLET_SPEED: f32 = 300.0;

/// A 4 cm thick wall across the X axis at x = 0.
fn thin_wall(app: &mut App, body: RigidBody) -> Entity {
    app.world_mut()
        .spawn((
            Transform::IDENTITY,
            RigidBody {
                gravity_scale: 0.0,
                ..body
            },
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::new(0.02, 2.0, 2.0),
                },
                ..Collider::default()
            },
        ))
        .id()
}

/// A 10 cm sphere 2.6 m before the wall flying +X: without a sweep its first step lands
/// at x = 2.4, its second at 7.4 (never inside the wall).
fn bullet(app: &mut App, ccd: bool, speed: f32) -> Entity {
    app.world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(-2.6, 0.0, 0.0)),
            RigidBody {
                ccd,
                gravity_scale: 0.0,
                ..RigidBody::dynamic()
            },
            Collider {
                shape: ColliderShape::Sphere { radius: 0.1 },
                restitution: 0.0,
                ..Collider::default()
            },
            Velocity {
                linear: Vec3::new(speed, 0.0, 0.0),
                angular: Vec3::ZERO,
            },
        ))
        .id()
}

#[test]
fn fast_body_does_not_tunnel_a_thin_static_wall_even_without_the_ccd_flag() {
    let mut app = physics_app();
    zero_gravity(&mut app);
    thin_wall(&mut app, RigidBody::static_body());
    let shot = bullet(&mut app, false, BULLET_SPEED);
    steps(&mut app, 30);
    let x = translation_of(&app, shot).x;
    assert!(x < 0.0, "fast bodies always sweep fixed geometry: x={x}");
}

#[test]
fn ccd_flag_is_what_stops_a_bullet_at_a_thin_kinematic_wall() {
    // Control: a plain fast body does not sweep kinematic bodies and passes the wall.
    let mut plain = physics_app();
    zero_gravity(&mut plain);
    thin_wall(&mut plain, RigidBody::kinematic());
    let plain_shot = bullet(&mut plain, false, BULLET_SPEED);
    steps(&mut plain, 30);
    let plain_x = translation_of(&plain, plain_shot).x;
    assert!(
        plain_x > 2.0,
        "without CCD the bullet jumps the 4 cm kinematic wall: x={plain_x}"
    );

    let mut swept = physics_app();
    zero_gravity(&mut swept);
    thin_wall(&mut swept, RigidBody::kinematic());
    let swept_shot = bullet(&mut swept, true, BULLET_SPEED);
    steps(&mut swept, 30);
    let swept_x = translation_of(&swept, swept_shot).x;
    assert!(swept_x < 0.0, "the CCD bullet stops at the wall: x={swept_x}");
}

#[test]
fn ccd_turned_on_at_runtime_takes_effect() {
    let mut app = physics_app();
    zero_gravity(&mut app);
    thin_wall(&mut app, RigidBody::kinematic());
    let shot = bullet(&mut app, false, 0.0);
    steps(&mut app, 2);
    {
        let world = app.world_mut();
        world.get_mut::<RigidBody>(shot).expect("body").ccd = true;
        world.get_mut::<Velocity>(shot).expect("velocity").linear = Vec3::new(BULLET_SPEED, 0.0, 0.0);
    }
    steps(&mut app, 30);
    assert!(
        rapier_body(&app, shot).is_ccd_enabled(),
        "the flag reached rapier"
    );
    let x = translation_of(&app, shot).x;
    assert!(x < 0.0, "the runtime CCD switch stops the bullet: x={x}");
}

#[test]
fn ccd_bullet_pushes_a_thin_dynamic_plate_instead_of_passing_it() {
    let mut app = physics_app();
    zero_gravity(&mut app);
    let plate = thin_wall(&mut app, RigidBody::dynamic());
    let shot = bullet(&mut app, true, BULLET_SPEED);
    steps(&mut app, 30);
    let plate_x = translation_of(&app, plate).x;
    let shot_x = translation_of(&app, shot).x;
    // Momentum: 4.2 g at 300 m/s into a 0.64 kg plate leaves both near 2 m/s, so the plate
    // travels about 0.9 m in the half second.
    assert!(
        plate_x > 0.2,
        "the hit carried the plate along +X: plate x={plate_x}"
    );
    assert!(
        shot_x < plate_x,
        "the bullet stayed behind the plate: bullet x={shot_x}, plate x={plate_x}"
    );
}

#[test]
fn ccd_sweeps_a_collider_on_a_child_entity() {
    let mut app = physics_app();
    zero_gravity(&mut app);
    thin_wall(&mut app, RigidBody::kinematic());
    let body = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(-2.6, 0.0, 0.0)),
            RigidBody {
                ccd: true,
                gravity_scale: 0.0,
                ..RigidBody::dynamic()
            },
            Velocity {
                linear: Vec3::new(BULLET_SPEED, 0.0, 0.0),
                angular: Vec3::ZERO,
            },
        ))
        .id();
    app.world_mut().spawn((
        Transform::IDENTITY,
        Collider {
            shape: ColliderShape::Sphere { radius: 0.1 },
            ..Collider::default()
        },
        ChildOf(body),
    ));
    steps(&mut app, 30);
    let x = translation_of(&app, body).x;
    assert!(x < 0.0, "the compound's child collider was swept: x={x}");
}

#[test]
fn fast_body_does_not_tunnel_a_zero_thickness_static_trimesh() {
    let mut app = physics_app();
    zero_gravity(&mut app);
    // One quad in the plane x = 0 (a tunnel wall or a mesh floor turned on its side), wound so
    // its face normal points -X, towards the bullet.
    app.world_mut().spawn((
        Transform::IDENTITY,
        RigidBody::static_body(),
        Collider {
            shape: ColliderShape::TriMesh {
                vertices: vec![
                    Vec3::new(0.0, -2.0, -2.0),
                    Vec3::new(0.0, 2.0, -2.0),
                    Vec3::new(0.0, 2.0, 2.0),
                    Vec3::new(0.0, -2.0, 2.0),
                ],
                indices: vec![[0, 2, 1], [0, 3, 2]],
            },
            ..Collider::default()
        },
    ));
    let shot = bullet(&mut app, false, BULLET_SPEED);
    steps(&mut app, 30);
    let x = translation_of(&app, shot).x;
    assert!(x < 0.0, "the sweep stops at the mesh: x={x}");
}

// ---------------------------------------------------------------------------
// Sleeping and waking
// ---------------------------------------------------------------------------

/// A box resting on the ground, asleep (3 s of settling; rapier sleeps after 0.5 s still).
fn sleeping_box(app: &mut App) -> (Entity, Entity) {
    let floor = ground(app);
    let crate_box = unit_box(app, Vec3::new(0.0, 0.5, 0.0));
    steps(app, 180);
    assert!(is_sleeping(app, crate_box), "the resting box fell asleep");
    (floor, crate_box)
}

#[test]
fn resting_box_falls_asleep_where_it_landed() {
    let mut app = physics_app();
    let (_, crate_box) = sleeping_box(&mut app);
    let y = translation_of(&app, crate_box).y;
    assert!((y - 0.5).abs() < 0.02, "asleep on the ground: y={y}");
}

#[test]
fn sleeping_body_holds_its_rendered_transform_bit_for_bit() {
    let mut app = physics_app();
    let (_, crate_box) = sleeping_box(&mut app);
    let before = *app.world().get::<Transform>(crate_box).expect("transform");
    steps(&mut app, 10);
    let after = *app.world().get::<Transform>(crate_box).expect("transform");
    assert_eq!(
        before.translation.to_array().map(f32::to_bits),
        after.translation.to_array().map(f32::to_bits),
        "no interpolation jitter while asleep"
    );
    assert_eq!(
        before.rotation.to_array().map(f32::to_bits),
        after.rotation.to_array().map(f32::to_bits)
    );
}

#[test]
fn can_sleep_false_keeps_a_resting_box_awake() {
    let mut app = physics_app();
    ground(&mut app);
    let awake = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, 0.5, 0.0)),
            RigidBody {
                can_sleep: false,
                ..RigidBody::dynamic()
            },
            Collider::default(),
        ))
        .id();
    steps(&mut app, 180);
    assert!(!is_sleeping(&app, awake), "can_sleep = false never sleeps");
}

#[test]
fn velocity_write_wakes_a_sleeping_box() {
    let mut app = physics_app();
    let (_, crate_box) = sleeping_box(&mut app);
    app.world_mut()
        .get_mut::<Velocity>(crate_box)
        .expect("velocity")
        .linear = Vec3::new(0.0, 4.0, 0.0);
    app.update();
    assert!(!is_sleeping(&app, crate_box), "the write woke it");
    // The stepped pose: the rendered `Transform` is interpolated and trails by one step.
    let y = rapier_body(&app, crate_box).translation().y;
    assert!(y > 0.53, "and it moved up this step: y={y}");
}

#[test]
fn impulse_wakes_a_sleeping_box() {
    let mut app = physics_app();
    let (_, crate_box) = sleeping_box(&mut app);
    app.world_mut().entity_mut(crate_box).insert(ExternalImpulse {
        impulse: Vec3::new(0.0, 5.0, 0.0),
        torque_impulse: Vec3::ZERO,
    });
    app.update();
    assert!(!is_sleeping(&app, crate_box), "the impulse woke it");
    let y = rapier_body(&app, crate_box).translation().y;
    assert!(y > 0.53, "and it moved up this step: y={y}");
}

#[test]
fn despawning_the_floor_wakes_a_sleeper_and_it_falls() {
    let mut app = physics_app();
    let (floor, crate_box) = sleeping_box(&mut app);
    app.world_mut().entity_mut(floor).despawn();
    steps(&mut app, 60);
    let y = translation_of(&app, crate_box).y;
    assert!(y < -1.0, "nothing holds it up any more: y={y}");
}

#[test]
fn lowering_a_kinematic_lift_wakes_its_sleeping_cargo() {
    let mut app = physics_app();
    let lift = app
        .world_mut()
        .spawn((
            Transform::IDENTITY,
            RigidBody::kinematic(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::new(2.0, 0.25, 2.0),
                },
                friction: 0.8,
                ..Collider::default()
            },
        ))
        .id();
    let cargo = unit_box(&mut app, Vec3::new(0.0, 0.75, 0.0));
    steps(&mut app, 180);
    assert!(is_sleeping(&app, cargo), "the cargo slept on the idle lift");

    // Lower the lift 1 m over 1 s.
    for _ in 0..60 {
        app.world_mut()
            .get_mut::<Transform>(lift)
            .expect("lift transform")
            .translation
            .y -= 1.0 / 60.0;
        app.update();
    }
    steps(&mut app, 30);
    let y = translation_of(&app, cargo).y;
    assert!(
        (y - (0.75 - 1.0)).abs() < 0.1,
        "the cargo followed the lift down instead of hanging in the air: y={y}"
    );
}

#[test]
fn gravity_change_wakes_sleeping_bodies() {
    let mut app = physics_app();
    let (_, crate_box) = sleeping_box(&mut app);
    // Flip gravity (no level loaded, so the settings' gravity applies).
    app.world_mut().resource_mut::<PhysicsSettings>().gravity = Vec3::new(0.0, 9.81, 0.0);
    steps(&mut app, 60);
    let y = translation_of(&app, crate_box).y;
    assert!(y > 2.0, "the box fell up under the new gravity: y={y}");
}

#[test]
fn a_character_walking_into_a_sleeping_crate_wakes_and_pushes_it() {
    let mut app = physics_app();
    let (_, crate_box) = sleeping_box(&mut app);
    // A 70 kg capsule (radius 0.3, half height 0.6) on the ground 1 m to the -X side of the
    // crate, walking +X at 2 m/s for half a second.
    let mut x = -1.0f32;
    for _ in 0..30 {
        let moved = {
            let mut physics = app.world_mut().resource_mut::<PhysicsWorld>();
            regia_physics::move_character(
                &mut physics,
                &CharacterBody::default(),
                0.3,
                0.6,
                (Vec3::new(x, 0.92, 0.0), Quat::IDENTITY),
                Vec3::new(2.0 / 60.0, 0.0, 0.0),
                0.8,
                0.0,
                0.0,
                None,
                1.0 / 60.0,
                1.0 / 60.0,
            )
        };
        x += moved.translation.x;
        app.update();
    }
    let crate_x = translation_of(&app, crate_box).x;
    assert!(
        crate_x > 0.05,
        "the push woke the crate and moved it: x={crate_x}"
    );
}

#[test]
fn a_settled_stack_sleeps_whole() {
    let mut app = physics_app();
    ground(&mut app);
    let stack: Vec<Entity> = (0..5)
        .map(|i| unit_box(&mut app, Vec3::new(0.0, 0.5 + i as f32, 0.0)))
        .collect();
    steps(&mut app, 300);
    for (i, &e) in stack.iter().enumerate() {
        assert!(is_sleeping(&app, e), "box {i} of the stack sleeps");
        let y = translation_of(&app, e).y;
        assert!(
            (y - (0.5 + i as f32)).abs() < 0.05,
            "box {i} still stacked: y={y}"
        );
    }
}

// ---------------------------------------------------------------------------
// Runtime body-kind changes and teleports keep gameplay control of the Transform
// ---------------------------------------------------------------------------

#[test]
fn body_switched_to_kinematic_follows_its_transform_then_falls_when_dynamic_again() {
    let mut app = physics_app();
    ground(&mut app);
    let prop = unit_box(&mut app, Vec3::new(0.0, 0.5, 0.0));
    steps(&mut app, 60);

    // Pick it up: kinematic, then carried to (3, 2, 0) by gameplay.
    app.world_mut().get_mut::<RigidBody>(prop).expect("body").kind = BodyKind::Kinematic;
    app.update();
    app.world_mut()
        .get_mut::<Transform>(prop)
        .expect("transform")
        .translation = Vec3::new(3.0, 2.0, 0.0);
    steps(&mut app, 3);
    let t = translation_of(&app, prop);
    assert!(
        (t - Vec3::new(3.0, 2.0, 0.0)).length() < 1e-4,
        "the carried prop keeps its gameplay Transform: {t}"
    );
    let body = rapier_body(&app, prop).translation();
    assert!(
        (body.x - 3.0).abs() < 1e-3 && (body.y - 2.0).abs() < 1e-3,
        "and the rapier body followed it: {body:?}"
    );

    // Drop it: dynamic again, it falls back onto the ground.
    app.world_mut().get_mut::<RigidBody>(prop).expect("body").kind = BodyKind::Dynamic;
    steps(&mut app, 120);
    let y = translation_of(&app, prop).y;
    assert!((y - 0.5).abs() < 0.05, "dropped back onto the ground: y={y}");
}

#[test]
fn teleported_kinematic_body_still_follows_its_transform() {
    let mut app = physics_app();
    let mover = app
        .world_mut()
        .spawn((Transform::IDENTITY, RigidBody::kinematic(), Collider::default()))
        .id();
    app.update();
    {
        let world = app.world_mut();
        world.get_mut::<Transform>(mover).expect("transform").translation = Vec3::new(5.0, 0.0, 0.0);
        world.entity_mut(mover).insert(Teleport);
    }
    app.update();
    app.world_mut()
        .get_mut::<Transform>(mover)
        .expect("transform")
        .translation = Vec3::new(6.0, 0.0, 0.0);
    steps(&mut app, 3);
    let t = translation_of(&app, mover);
    assert!((t.x - 6.0).abs() < 1e-4, "the Transform still drives it: {t}");
    let body = rapier_body(&app, mover).translation();
    assert!((body.x - 6.0).abs() < 1e-3, "rapier followed: {body:?}");
}

// ---------------------------------------------------------------------------
// Removing a touching collider still ends its contact / trigger overlap
// ---------------------------------------------------------------------------

/// A static 4 m sensor cube at the origin and a weightless box inside it.
fn trigger_with_occupant(app: &mut App) -> (Entity, Entity) {
    let trigger = app
        .world_mut()
        .spawn((
            Transform::IDENTITY,
            RigidBody::static_body(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::splat(2.0),
                },
                sensor: true,
                active_events: ActiveEvents {
                    collision: true,
                    contact_force: false,
                },
                ..Collider::default()
            },
        ))
        .id();
    let occupant = app
        .world_mut()
        .spawn((
            Transform::IDENTITY,
            RigidBody {
                gravity_scale: 0.0,
                ..RigidBody::dynamic()
            },
            Collider::default(),
        ))
        .id();
    (trigger, occupant)
}

#[test]
fn despawning_an_occupant_raises_trigger_exit() {
    let mut app = physics_app();
    let (trigger, occupant) = trigger_with_occupant(&mut app);
    let mut enter = MessageCursor::<TriggerEnter>::default();
    let mut exit = MessageCursor::<TriggerExit>::default();
    let mut entered = Vec::new();
    for _ in 0..5 {
        app.update();
        entered.extend(read_new(&app, &mut enter));
    }
    assert_eq!(
        entered,
        vec![TriggerEnter {
            trigger,
            other: occupant
        }]
    );

    app.world_mut().entity_mut(occupant).despawn();
    let mut exited = Vec::new();
    for _ in 0..5 {
        app.update();
        exited.extend(read_new(&app, &mut exit));
    }
    assert_eq!(
        exited,
        vec![TriggerExit {
            trigger,
            other: occupant
        }],
        "the despawned occupant left the trigger exactly once"
    );
}

#[test]
fn despawning_a_trigger_raises_trigger_exit_for_its_occupant() {
    let mut app = physics_app();
    let (trigger, occupant) = trigger_with_occupant(&mut app);
    let mut exit = MessageCursor::<TriggerExit>::default();
    steps(&mut app, 5);
    app.world_mut().entity_mut(trigger).despawn();
    let mut exited = Vec::new();
    for _ in 0..5 {
        app.update();
        exited.extend(read_new(&app, &mut exit));
    }
    assert_eq!(
        exited,
        vec![TriggerExit {
            trigger,
            other: occupant
        }],
        "the removed sensor is still the trigger side of the exit"
    );
}

#[test]
fn despawning_a_resting_body_raises_collision_ended() {
    let mut app = physics_app();
    let floor = ground(&mut app);
    let crate_box = unit_box(&mut app, Vec3::new(0.0, 0.5, 0.0));
    app.world_mut()
        .get_mut::<Collider>(crate_box)
        .expect("collider")
        .active_events = ActiveEvents {
        collision: true,
        contact_force: false,
    };
    let mut started = MessageCursor::<CollisionStarted>::default();
    let mut ended = MessageCursor::<CollisionEnded>::default();
    // Messages live two updates, so read after every step (the start comes on the first).
    let mut starts = 0;
    for _ in 0..30 {
        app.update();
        starts += read_new(&app, &mut started).len();
        assert!(
            read_new(&app, &mut ended).is_empty(),
            "the box rests on the floor"
        );
    }
    assert_eq!(starts, 1, "touching the floor");

    app.world_mut().entity_mut(crate_box).despawn();
    let mut ends = Vec::new();
    for _ in 0..5 {
        app.update();
        ends.extend(read_new(&app, &mut ended));
    }
    assert_eq!(ends.len(), 1, "exactly one end for the despawned box: {ends:?}");
    let pair = [ends[0].entity_a, ends[0].entity_b];
    assert!(
        pair.contains(&floor) && pair.contains(&crate_box),
        "the end names both participants: {pair:?}"
    );
}

#[test]
fn rebuilding_a_touching_collider_keeps_starts_and_ends_balanced() {
    let mut app = physics_app();
    ground(&mut app);
    let crate_box = unit_box(&mut app, Vec3::new(0.0, 0.5, 0.0));
    app.world_mut()
        .get_mut::<Collider>(crate_box)
        .expect("collider")
        .active_events = ActiveEvents {
        collision: true,
        contact_force: false,
    };
    let mut started = MessageCursor::<CollisionStarted>::default();
    let mut ended = MessageCursor::<CollisionEnded>::default();
    let (mut starts, mut ends) = (0, 0);
    for step in 0..120 {
        if step == 60 {
            // A material edit rebuilds the rapier collider while it touches the floor.
            app.world_mut()
                .get_mut::<Collider>(crate_box)
                .expect("collider")
                .friction = 0.3;
        }
        app.update();
        starts += read_new(&app, &mut started).len();
        ends += read_new(&app, &mut ended).len();
    }
    assert_eq!(
        starts,
        ends + 1,
        "every start but the current contact has its end ({starts} starts, {ends} ends)"
    );
}

// ---------------------------------------------------------------------------
// Joint stability
// ---------------------------------------------------------------------------

/// A static anchor at `top` and `links` boxes (half 0.1) hanging below it, jointed with
/// revolute joints about Z, anchors ±0.25 m from each link centre. Returns the joint pairs
/// as (carrier, connected, anchor, connected_anchor).
fn hanging_chain(
    app: &mut App,
    top: Vec3,
    links: usize,
    density_of: impl Fn(usize) -> f32,
) -> Vec<(Entity, Entity, Vec3, Vec3)> {
    let anchor = app
        .world_mut()
        .spawn((
            Transform::from_translation(top),
            RigidBody::static_body(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::splat(0.1),
                },
                ..Collider::default()
            },
        ))
        .id();
    let mut joints = Vec::new();
    let mut previous = anchor;
    for i in 0..links {
        let link = app
            .world_mut()
            .spawn((
                Transform::from_translation(top - Vec3::Y * (0.5 * (i as f32 + 1.0))),
                RigidBody::dynamic(),
                Collider {
                    shape: ColliderShape::Box {
                        half_extents: Vec3::splat(0.1),
                    },
                    density: density_of(i),
                    ..Collider::default()
                },
                Velocity::default(),
            ))
            .id();
        let (a, b) = (Vec3::new(0.0, -0.25, 0.0), Vec3::new(0.0, 0.25, 0.0));
        app.world_mut().entity_mut(previous).insert((
            Joint {
                kind: JointKind::Revolute {
                    axis: Vec3::Z,
                    limits: None,
                    motor: None,
                },
                connected: link,
                anchor: a,
                connected_anchor: b,
                break_force: 0.0,
            },
            JointNoContacts,
        ));
        joints.push((previous, link, a, b));
        previous = link;
    }
    joints
}

/// The largest distance between the two world anchors of any joint.
fn max_joint_gap(app: &App, joints: &[(Entity, Entity, Vec3, Vec3)]) -> f32 {
    joints
        .iter()
        .map(|&(a, b, anchor_a, anchor_b)| {
            (world_anchor(app, a, anchor_a) - world_anchor(app, b, anchor_b)).length()
        })
        .fold(0.0, f32::max)
}

#[test]
fn twenty_link_chain_swings_without_coming_apart() {
    let mut app = physics_app();
    let joints = hanging_chain(&mut app, Vec3::new(0.0, 12.0, 0.0), 20, |_| 1.0);
    let tail = joints[joints.len() - 1].1;
    // Kick the tail sideways so the whole chain whips.
    app.world_mut()
        .get_mut::<Velocity>(tail)
        .expect("velocity")
        .linear = Vec3::new(6.0, 0.0, 0.0);
    let mut worst = 0.0f32;
    for _ in 0..600 {
        app.update();
        worst = worst.max(max_joint_gap(&app, &joints));
    }
    assert!(worst.is_finite(), "no NaN in the chain");
    // A link that came off would leave a gap of half a metre or more.
    assert!(worst < 0.1, "links stay pinned together: worst gap {worst} m");
    assert_eq!(
        app.world().resource::<PhysicsWorld>().impulse_joints.len(),
        20,
        "every joint is intact"
    );
}

#[test]
fn heavy_end_mass_on_a_light_chain_stays_bounded() {
    let mut app = physics_app();
    // Five links of 8 g and a 100× heavier last link (0.8 kg).
    let joints = hanging_chain(&mut app, Vec3::new(0.0, 6.0, 0.0), 6, |i| {
        if i == 5 { 100.0 } else { 1.0 }
    });
    let tail = joints[5].1;
    app.world_mut()
        .get_mut::<Velocity>(tail)
        .expect("velocity")
        .linear = Vec3::new(3.0, 0.0, 0.0);
    let mut worst = 0.0f32;
    for _ in 0..600 {
        app.update();
        worst = worst.max(max_joint_gap(&app, &joints));
    }
    assert!(worst.is_finite(), "no NaN under a 100:1 mass ratio");
    assert!(
        worst < 0.25,
        "the light links stretch but hold: worst gap {worst} m"
    );
    // At rest the tail hangs 3 m below the anchor; six stretched gaps cost at most 1.5 m.
    let tail_y = translation_of(&app, tail).y;
    assert!(
        tail_y > 1.5,
        "the heavy end still hangs from the chain: y={tail_y}"
    );
}

#[test]
fn revolute_limit_holds_a_hinged_arm_against_gravity() {
    let mut app = physics_app();
    let pivot = Vec3::new(0.0, 3.0, 0.0);
    let hinge = app
        .world_mut()
        .spawn((Transform::from_translation(pivot), RigidBody::static_body()))
        .id();
    // A 1 m arm sticking out +X from the hinge, free to rotate ±0.3 rad about Z.
    let arm = app
        .world_mut()
        .spawn((
            Transform::from_translation(pivot + Vec3::X * 0.5),
            RigidBody::dynamic(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::new(0.5, 0.05, 0.05),
                },
                ..Collider::default()
            },
        ))
        .id();
    app.world_mut().entity_mut(hinge).insert(Joint {
        kind: JointKind::Revolute {
            axis: Vec3::Z,
            limits: Some((-0.3, 0.3)),
            motor: None,
        },
        connected: arm,
        anchor: Vec3::ZERO,
        connected_anchor: Vec3::new(-0.5, 0.0, 0.0),
        break_force: 0.0,
    });
    let mut lowest = 0.0f32;
    for _ in 0..180 {
        app.update();
        let r = rotation_of(&app, arm);
        let angle = 2.0 * r.z.atan2(r.w);
        lowest = lowest.min(angle);
    }
    let r = rotation_of(&app, arm);
    let angle = 2.0 * r.z.atan2(r.w);
    // The limit engages once crossed: about one step of overshoot (≈ 0.05 rad) at most.
    assert!(
        lowest > -0.3 - 0.1,
        "the arm never swung far past the limit: {lowest}"
    );
    assert!(
        (angle + 0.3).abs() < 0.06,
        "gravity rests the arm on its lower limit: {angle} rad"
    );
}

#[test]
fn prismatic_limit_stops_a_falling_slider() {
    let mut app = physics_app();
    let rail = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, 3.0, 0.0)),
            RigidBody::static_body(),
        ))
        .id();
    let slider = unit_box(&mut app, Vec3::new(0.0, 3.0, 0.0));
    app.world_mut().entity_mut(rail).insert((
        Joint {
            kind: JointKind::Prismatic {
                axis: Vec3::Y,
                limits: Some((-1.0, 0.0)),
                motor: None,
            },
            connected: slider,
            anchor: Vec3::ZERO,
            connected_anchor: Vec3::ZERO,
            break_force: 0.0,
        },
        JointNoContacts,
    ));
    steps(&mut app, 180);
    let t = translation_of(&app, slider);
    assert!(
        (t.y - 2.0).abs() < 0.05,
        "the slider rests on its -1 m limit: {t}"
    );
    assert!(t.x.abs() < 0.01 && t.z.abs() < 0.01, "and stays on the rail: {t}");
}

#[test]
fn spherical_cone_limit_bounds_the_swing() {
    let mut app = physics_app();
    let pivot = Vec3::new(0.0, 5.0, 0.0);
    let socket = app
        .world_mut()
        .spawn((Transform::from_translation(pivot), RigidBody::static_body()))
        .id();
    // A 1 m rod hanging along -Y from the socket.
    let rod = app
        .world_mut()
        .spawn((
            Transform::from_translation(pivot - Vec3::Y * 0.5),
            RigidBody::dynamic(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::new(0.05, 0.5, 0.05),
                },
                ..Collider::default()
            },
            Velocity::default(),
        ))
        .id();
    app.world_mut().entity_mut(socket).insert(Joint {
        kind: JointKind::Spherical {
            cone_limit: Some(0.3),
        },
        connected: rod,
        anchor: Vec3::ZERO,
        connected_anchor: Vec3::new(0.0, 0.5, 0.0),
        break_force: 0.0,
    });
    // A kick along +X (4 rad/s about the socket): unlimited, the rod swings to about
    // 1.1 rad; the limit engages once crossed, so allow about one step of overshoot.
    app.world_mut().get_mut::<Velocity>(rod).expect("velocity").linear = Vec3::new(2.0, 0.0, 0.0);
    let mut widest = 0.0f32;
    for _ in 0..180 {
        app.update();
        let axis = rotation_of(&app, rod) * Vec3::Y;
        widest = widest.max(axis.dot(Vec3::Y).clamp(-1.0, 1.0).acos());
    }
    assert!(widest > 0.2, "the kick reached the cone: {widest} rad");
    assert!(widest < 0.3 + 0.15, "the cone held the swing: {widest} rad");
}

#[test]
fn revolute_motor_spins_a_wheel_to_its_target_velocity() {
    let mut app = physics_app();
    zero_gravity(&mut app);
    let axle = app
        .world_mut()
        .spawn((Transform::IDENTITY, RigidBody::static_body()))
        .id();
    let wheel = app
        .world_mut()
        .spawn((
            Transform::IDENTITY,
            RigidBody::dynamic(),
            Collider {
                shape: ColliderShape::Sphere { radius: 0.5 },
                ..Collider::default()
            },
            Velocity::default(),
        ))
        .id();
    app.world_mut().entity_mut(axle).insert((
        Joint {
            kind: JointKind::Revolute {
                axis: Vec3::Z,
                limits: None,
                motor: Some(JointMotorConfig {
                    target_position: 0.0,
                    target_velocity: 2.0,
                    stiffness: 0.0,
                    damping: 50.0,
                    max_force: 0.0,
                }),
            },
            connected: wheel,
            anchor: Vec3::ZERO,
            connected_anchor: Vec3::ZERO,
            break_force: 0.0,
        },
        JointNoContacts,
    ));
    steps(&mut app, 120);
    let spin = app.world().get::<Velocity>(wheel).expect("velocity").angular;
    assert!(
        (spin.z.abs() - 2.0).abs() < 0.2,
        "the motor holds 2 rad/s about the axle: {spin}"
    );
    assert!(
        spin.x.abs() < 0.05 && spin.y.abs() < 0.05,
        "and only about it: {spin}"
    );
}

#[test]
fn damped_chain_comes_to_rest_and_sleeps() {
    let mut app = physics_app();
    let joints = hanging_chain(&mut app, Vec3::new(0.0, 5.0, 0.0), 5, |_| 1.0);
    for &(_, link, _, _) in &joints {
        let mut body = app.world_mut().get_mut::<RigidBody>(link).expect("body");
        body.linear_damping = 0.5;
        body.angular_damping = 2.0;
    }
    let tail = joints[4].1;
    app.world_mut()
        .get_mut::<Velocity>(tail)
        .expect("velocity")
        .linear = Vec3::new(2.0, 0.0, 0.0);
    steps(&mut app, 900);
    for (i, &(_, link, _, _)) in joints.iter().enumerate() {
        assert!(is_sleeping(&app, link), "link {i} sleeps once the swing died out");
    }
}

#[test]
fn despawning_a_middle_link_drops_its_joints_and_frees_the_tail() {
    let mut app = physics_app();
    let joints = hanging_chain(&mut app, Vec3::new(0.0, 5.0, 0.0), 3, |_| 1.0);
    steps(&mut app, 30);
    assert_eq!(app.world().resource::<PhysicsWorld>().impulse_joints.len(), 3);
    let middle = joints[1].0;
    let tail = joints[2].1;
    let tail_y = translation_of(&app, tail).y;
    app.world_mut().entity_mut(middle).despawn();
    steps(&mut app, 30);
    assert_eq!(
        app.world().resource::<PhysicsWorld>().impulse_joints.len(),
        1,
        "only the anchor → first link joint is left"
    );
    let fallen = translation_of(&app, tail).y;
    assert!(fallen < tail_y - 1.0, "the tail fell free: {tail_y} → {fallen}");
}

// ---------------------------------------------------------------------------
// Solver settings (PhysicsSettings::solver_iterations / ccd_substeps)
// ---------------------------------------------------------------------------

/// The worst joint gap of a kicked 6-link chain with a 100× heavier end, at `iterations`.
fn heavy_chain_worst_gap(iterations: usize) -> f32 {
    let mut app = physics_app();
    app.insert_resource(PhysicsSettings {
        solver_iterations: iterations,
        ..PhysicsSettings::default()
    });
    let joints = hanging_chain(&mut app, Vec3::new(0.0, 6.0, 0.0), 6, |i| {
        if i == 5 { 100.0 } else { 1.0 }
    });
    let tail = joints[5].1;
    app.world_mut()
        .get_mut::<Velocity>(tail)
        .expect("velocity")
        .linear = Vec3::new(3.0, 0.0, 0.0);
    let mut worst = 0.0f32;
    for _ in 0..300 {
        app.update();
        worst = worst.max(max_joint_gap(&app, &joints));
    }
    worst
}

#[test]
fn more_solver_iterations_stretch_a_heavy_chain_less() {
    let default_gap = heavy_chain_worst_gap(PhysicsSettings::default().solver_iterations);
    let precise_gap = heavy_chain_worst_gap(16);
    assert!(
        precise_gap < default_gap,
        "16 iterations hold the links tighter than 4: {precise_gap} m vs {default_gap} m"
    );
}

/// Audit v033b (High): a dynamic body under a parent whose scale is zero. The parent's inverse is
/// NaN, and the step wrote the NaN local pose into the body's `Transform` (and the frame
/// interpolation wrote it again). The `Transform` keeps a finite value and the drop is counted.
#[test]
fn a_body_under_a_zero_scale_parent_keeps_a_finite_transform() {
    let mut app = physics_app();
    let parent = app
        .world_mut()
        .spawn((
            Transform {
                scale: Vec3::ZERO,
                ..Transform::IDENTITY
            },
            regia_scene::GlobalTransform(regia_core::math::Affine3A::from_scale(Vec3::ZERO)),
        ))
        .id();
    let body = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, 1.0, 0.0)),
            RigidBody::dynamic(),
            Collider {
                shape: ColliderShape::Sphere { radius: 0.5 },
                ..Collider::default()
            },
            ChildOf(parent),
        ))
        .id();
    steps(&mut app, 5);
    let transform = *app.world().get::<Transform>(body).expect("transform");
    assert!(
        transform.translation.is_finite() && transform.rotation.is_finite(),
        "{transform:?}"
    );
    assert!(app.world().resource::<PhysicsWorld>().stats().non_finite_inputs > 0);
}
