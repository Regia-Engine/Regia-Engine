// SPDX-License-Identifier: MPL-2.0
//! A rapier2d box stack settles, stays upright and settles to the same bits every run
//! (`docs/specs/22-tier2.md` § T2.10 acceptance).
#![cfg(feature = "physics2d")]

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_physics::physics2d::{Collider2d, Physics2dPlugin, PhysicsWorld2d, RigidBody2d, Velocity2d};
use regia_scene::Transform;

const BOXES: usize = 10;

/// Builds the scene, steps it `steps` times at 60 Hz and returns each box's (x, y, angle) bits,
/// plus whether every body is asleep.
fn run(steps: usize) -> (Vec<[u32; 3]>, bool, Vec<Vec3>) {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, Physics2dPlugin));
    enable_fixed_step(&mut app, 60.0);
    app.world_mut().spawn((
        Collider2d::cuboid(40.0, 2.0),
        Transform::from_translation(Vec3::new(0.0, -1.0, 0.0)),
    ));
    let boxes: Vec<Entity> = (0..BOXES)
        .map(|i| {
            app.world_mut()
                .spawn((
                    RigidBody2d::default(),
                    Collider2d::cuboid(1.0, 1.0),
                    // Small gaps and alternating offsets: the stack has to settle, not start at rest.
                    Transform::from_translation(Vec3::new(
                        if i % 2 == 0 { 0.02 } else { -0.02 },
                        0.5 + i as f32 * 1.05,
                        0.0,
                    )),
                ))
                .id()
        })
        .collect();
    for _ in 0..steps {
        app.update();
    }
    let world = app.world();
    let poses: Vec<Vec3> = boxes
        .iter()
        .map(|e| {
            world
                .get::<Transform>(*e)
                .map(|t| t.translation)
                .unwrap_or_default()
        })
        .collect();
    let bits = boxes
        .iter()
        .map(|e| {
            let t = world.get::<Transform>(*e).copied().unwrap_or_default();
            let (z, _, _) = t.rotation.to_euler(regia_core::glam::EulerRot::ZYX);
            [t.translation.x.to_bits(), t.translation.y.to_bits(), z.to_bits()]
        })
        .collect();
    let speeds_ok = boxes.iter().all(|e| {
        world
            .get::<Velocity2d>(*e)
            .is_some_and(|v| v.linear.length() < 1e-3 && v.angular.abs() < 1e-3)
    });
    let asleep = world.resource::<PhysicsWorld2d>().all_asleep() && speeds_ok;
    (bits, asleep, poses)
}

#[test]
fn box_stack_settles_upright_and_deterministically() {
    let (first, asleep, poses) = run(600);
    assert!(asleep, "the stack comes to rest within 10 s");
    for (i, p) in poses.iter().enumerate() {
        assert!(p.x.abs() < 0.1, "box {i} drifted sideways: {p}");
        let expected = 0.5 + i as f32;
        assert!(
            (p.y - expected).abs() < 0.05,
            "box {i} at {p}, expected y ≈ {expected}"
        );
    }
    // Same build, same inputs → the same bits, run after run.
    for _ in 0..2 {
        let (again, _, _) = run(600);
        assert_eq!(again, first);
    }
}

/// Two balls dropped from the same height onto a floor: both contacts begin on the same
/// step. With `invert_ids`, both entity ids are reserved first (as a parallel system's
/// earlier-recorded command queue would reserve them) and the components are applied
/// left-before-right, so the entity-id order is the reverse of the collider arrival order —
/// what another thread count produces (ADR-1347). Returns the begin/end sequence with
/// entities mapped to scene roles (the ids legitimately differ between the timings).
fn contact_sequence(invert_ids: bool) -> Vec<(&'static str, &'static str, bool)> {
    use regia_ecs::ecs::message::{MessageCursor, Messages};

    let mut app = App::new();
    app.add_plugins((MinimalPlugins, Physics2dPlugin));
    enable_fixed_step(&mut app, 60.0);
    let floor = app
        .world_mut()
        .spawn((
            Collider2d::cuboid(40.0, 1.0),
            Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
        ))
        .id();
    let ball = |x: f32| {
        (
            RigidBody2d::default(),
            Collider2d::ball(0.5),
            Transform::from_translation(Vec3::new(x, 2.0, 0.0)),
        )
    };
    let (left_id, right_id) = if invert_ids {
        let mut left = bevy_ecs::world::CommandQueue::default();
        let mut right = bevy_ecs::world::CommandQueue::default();
        let world = app.world_mut();
        // Recorded first: reserves the lower entity id.
        let right_id = Commands::new(&mut right, world).spawn(ball(0.3)).id();
        let left_id = Commands::new(&mut left, world).spawn(ball(-0.3)).id();
        // Applied in system order: the left ball still arrives first.
        left.apply(world);
        right.apply(world);
        (left_id, right_id)
    } else {
        (
            app.world_mut().spawn(ball(-0.3)).id(),
            app.world_mut().spawn(ball(0.3)).id(),
        )
    };
    let role = |e: Entity| {
        if e == left_id {
            "left"
        } else if e == right_id {
            "right"
        } else if e == floor {
            "floor"
        } else {
            "other"
        }
    };
    let mut out = Vec::new();
    let mut cursor = MessageCursor::<regia_physics::physics2d::Collision2d>::default();
    for _ in 0..90 {
        app.update();
        let messages = app
            .world()
            .resource::<Messages<regia_physics::physics2d::Collision2d>>();
        out.extend(cursor.read(messages).map(|m| (role(m.a), role(m.b), m.started)));
    }
    out
}

/// `Collision2d` begins must follow the rapier collider-handle (arrival) order, not the
/// entity-id order: pre-fix the pair set was keyed by `(min(entity), max(entity))`, so a
/// run with the ids swapped between the balls delivered the two simultaneous begins in the
/// other sequence and swapped `a`/`b`.
#[test]
fn collision2d_begins_follow_collider_handle_order_not_entity_ids() {
    let natural = contact_sequence(false);
    let inverted = contact_sequence(true);
    // The balls overlap each other mid-air and both land on the floor: three begins, and
    // the contacts end again when the run stops ... or at least the sequence is non-trivial.
    let began = |seq: &Vec<(&str, &str, bool)>| seq.iter().filter(|c| c.2).count();
    assert_eq!(
        began(&natural),
        3,
        "two floor contacts plus the ball-ball contact: {natural:?}"
    );
    assert_eq!(
        natural, inverted,
        "begin/end order must follow collider handles, not ids"
    );
}
