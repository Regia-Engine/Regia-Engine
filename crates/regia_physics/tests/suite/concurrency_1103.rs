// SPDX-License-Identifier: MPL-2.0
//! Regressions from the 11-03 ECS concurrency audit (ADR-2396) whose code lives in
//! `regia_physics`: marker removals survive frames without a fixed step, and rapier objects are
//! removed in spawn order, not entity-id order.

use std::time::Duration;

use regia_app::{App, FixedTime, TimeUpdateStrategy};
use regia_core::math::Vec3;
use regia_ecs::prelude::Entity;
use regia_physics::{
    Collider, ColliderShape, Joint, JointHandle, JointKind, JointNoContacts, PhysicsPlugin, PhysicsWorld,
    RigidBody,
};
use regia_scene::Transform;

fn physics_app() -> App {
    let mut app = App::new();
    set_frame(&mut app, 1.0 / 60.0);
    let mut fixed = FixedTime::default();
    fixed.hz = 60.0;
    app.insert_resource(fixed).add_plugins(PhysicsPlugin);
    app
}

fn set_frame(app: &mut App, seconds: f64) {
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        seconds,
    )));
}

fn run(app: &mut App, frames: u32) {
    for _ in 0..frames {
        app.update();
    }
}

fn box_collider(half: f32) -> Collider {
    Collider {
        shape: ColliderShape::Box {
            half_extents: Vec3::splat(half),
        },
        ..Collider::default()
    }
}

fn spawn_body(app: &mut App, kind: RigidBody, translation: Vec3) -> Entity {
    app.world_mut()
        .spawn((Transform::from_translation(translation), kind, box_collider(0.5)))
        .id()
}

fn contacts_enabled(app: &App, joint: Entity) -> bool {
    let handle = app
        .world()
        .get::<JointHandle>(joint)
        .expect("joint handle")
        .to_handle();
    app.world()
        .resource::<PhysicsWorld>()
        .impulse_joints
        .get(handle)
        .expect("joint")
        .data
        .contacts_enabled
}

/// Audit 11-03 ecs H1: `sync_joints` (a `FixedUpdate` system) read `JointNoContacts` removals
/// only from `RemovedComponents`, which the app clears every frame. With a render rate above
/// twice the fixed rate, frames without a fixed step dropped the removal and the joint kept
/// contacts off for good.
#[test]
fn a_marker_removed_between_fixed_steps_still_enables_contacts() {
    let mut app = physics_app();
    let a = spawn_body(&mut app, RigidBody::dynamic(), Vec3::ZERO);
    let b = spawn_body(&mut app, RigidBody::dynamic(), Vec3::new(0.0, 3.0, 0.0));
    app.world_mut().entity_mut(a).insert((
        Joint {
            kind: JointKind::Distance { min: 0.0, max: 2.0 },
            connected: b,
            anchor: Vec3::ZERO,
            connected_anchor: Vec3::ZERO,
            break_force: 0.0,
        },
        JointNoContacts,
    ));
    run(&mut app, 2);
    assert!(!contacts_enabled(&app, a), "the marker disables contacts");

    // 240 Hz frames against 60 Hz physics: three frames (12.5 ms) run no fixed step, so the
    // removal message ages out of both buffers before `sync_joints` runs again.
    set_frame(&mut app, 1.0 / 240.0);
    app.world_mut().entity_mut(a).remove::<JointNoContacts>();
    run(&mut app, 3);
    set_frame(&mut app, 1.0 / 60.0);
    run(&mut app, 2);
    assert!(
        contacts_enabled(&app, a),
        "removing the marker re-enables contacts even when no fixed step ran that frame"
    );
}

/// Spawns two entities, then gives them their physics components in `first`, `second` order:
/// spawn order (component arrival) and entity-id order disagree when `first` is the later id.
fn arrive_in_order(app: &mut App, swap_ids: bool, bundle: impl Fn(&mut App, Entity)) -> (Entity, Entity) {
    let low = app.world_mut().spawn_empty().id();
    let high = app.world_mut().spawn_empty().id();
    let (first, second) = if swap_ids { (high, low) } else { (low, high) };
    bundle(app, first);
    bundle(app, second);
    (first, second)
}

/// The rapier body handle that the next spawned body gets after two debris chunks that arrived
/// in the same order expire on the same step. `swap_ids` only changes which entity ids the
/// chunks got (what parallel `Commands::spawn` does with thread timing).
fn handle_after_debris_expiry(swap_ids: bool) -> (u32, u32) {
    let mut app = physics_app();
    let (first, second) = arrive_in_order(&mut app, swap_ids, |app, entity| {
        app.world_mut().entity_mut(entity).insert((
            Transform::from_translation(Vec3::new(0.0, 10.0, 0.0)),
            RigidBody::dynamic(),
            box_collider(0.2),
            regia_physics::DebrisTag {
                spawn_time_secs: None,
                lifetime_secs: 0.05,
            },
        ));
    });
    run(&mut app, 8);
    assert!(
        app.world().get_entity(first).is_err() && app.world().get_entity(second).is_err(),
        "both chunks expired"
    );
    let next = spawn_body(&mut app, RigidBody::dynamic(), Vec3::new(5.0, 0.0, 0.0));
    run(&mut app, 1);
    app.world()
        .get::<regia_physics::RigidBodyHandle>(next)
        .expect("body handle")
        .to_handle()
        .into_raw_parts()
}

/// Audit 11-03 ecs M1: debris expiry removed rapier bodies in entity-id order. Rapier reuses
/// freed slots in removal order, so the next body's handle (and solve order) depended on which
/// ids the chunks got, not on the order they arrived.
#[test]
fn debris_expiry_frees_handles_in_arrival_order_whatever_the_entity_ids() {
    assert_eq!(
        handle_after_debris_expiry(false),
        handle_after_debris_expiry(true),
        "the next body's handle must not depend on the expired chunks' entity ids"
    );
}

/// The two actors' platformer handles, in arrival order.
fn platformer_handles(swap_ids: bool) -> (u32, u32) {
    use regia_physics::platformer2d::{Actor, PlatformerActor, PlatformerPlugin};
    let mut app = App::new();
    set_frame(&mut app, 1.0 / 60.0);
    let mut fixed = FixedTime::default();
    fixed.hz = 60.0;
    app.insert_resource(fixed)
        .add_plugins((PlatformerPlugin, PhysicsPlugin));
    let (first, second) = arrive_in_order(&mut app, swap_ids, |app, entity| {
        app.world_mut()
            .entity_mut(entity)
            .insert(PlatformerActor::default());
    });
    run(&mut app, 2);
    let handle = |e: Entity| app.world().get::<Actor>(e).expect("actor handle").0;
    (handle(first), handle(second))
}

/// Audit 11-03 ecs M3: platformer actors were numbered in entity-id order, so actors spawned by
/// unordered parallel systems got their handles (and move order) by thread timing.
#[test]
fn platformer_actors_are_numbered_in_arrival_order_whatever_the_entity_ids() {
    let in_id_order = platformer_handles(false);
    let against_id_order = platformer_handles(true);
    assert!(in_id_order.0 < in_id_order.1, "{in_id_order:?}");
    assert_eq!(
        in_id_order, against_id_order,
        "the handles follow arrival order, not entity ids"
    );
}
