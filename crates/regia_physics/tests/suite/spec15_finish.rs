// SPDX-License-Identifier: MPL-2.0
//! Spec 15 remainder (ADR-2424): joint break by torque, the debug draw's cost when off, and
//! named physics materials. Fixed 60 Hz steps from deterministic inputs; no wall-clock budgets.

use std::time::Duration;

use regia_app::{App, FixedTime, TimeUpdateStrategy};
use regia_core::math::Vec3;
use regia_ecs::ecs::message::{MessageCursor, Messages};
use regia_ecs::prelude::Entity;
use regia_physics::debug_draw::{PhysicsDebugDraw, PhysicsDebugKinds};
use regia_physics::{
    Collider, ColliderShape, CombineRule, Joint, JointBreakTorque, JointBroken, JointKind, PhysicsMaterial,
    PhysicsMaterialRef, PhysicsMaterials, PhysicsPlugin, PhysicsWorld, RigidBody, Velocity,
};
use regia_scene::Transform;

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

fn boxed(app: &mut App, body: RigidBody, half: Vec3, at: Vec3) -> Entity {
    app.world_mut()
        .spawn((
            Transform::from_translation(at),
            body,
            Collider {
                shape: ColliderShape::Box { half_extents: half },
                ..Collider::default()
            },
            Velocity::default(),
        ))
        .id()
}

/// A static anchor and a dynamic block held by a fixed joint at a 1 m lever arm: gravity on the
/// block (the joint point is 1 m from its center) is a steady torque of `m g r` on the joint. `break_torque` / `break_force` are the limits.
fn lever(app: &mut App, break_force: f32, break_torque: f32) -> Entity {
    let anchor = boxed(
        app,
        RigidBody::static_body(),
        Vec3::splat(0.1),
        Vec3::new(0.0, 5.0, 0.0),
    );
    let block = boxed(
        app,
        RigidBody::dynamic(),
        Vec3::splat(0.2),
        Vec3::new(1.0, 5.0, 0.0),
    );
    let mut joint = app.world_mut().entity_mut(anchor);
    joint.insert(Joint {
        kind: JointKind::Fixed,
        connected: block,
        anchor: Vec3::ZERO,
        connected_anchor: Vec3::new(-1.0, 0.0, 0.0),
        break_force,
    });
    if break_torque > 0.0 {
        joint.insert(JointBreakTorque(break_torque));
    }
    anchor
}

/// The update (1-based) after which the first `JointBroken` was seen, within `limit` steps.
fn first_break(app: &mut App, limit: usize) -> Option<usize> {
    let mut cursor = MessageCursor::<JointBroken>::default();
    for step in 1..=limit {
        app.update();
        let messages = app.world().resource::<Messages<JointBroken>>();
        if cursor.read(messages).next().is_some() {
            return Some(step);
        }
    }
    None
}

#[test]
fn a_torque_limit_breaks_a_lever_at_a_fixed_step_and_the_run_repeats() {
    let run = || {
        let mut app = physics_app();
        lever(&mut app, 0.0, 0.2);
        first_break(&mut app, 120)
    };
    let a = run();
    assert!(
        a.is_some(),
        "a 0.2 N m limit must break: the 64 g block hangs 1 m out, about 0.63 N m"
    );
    assert_eq!(a, run(), "the break step is deterministic");
}

#[test]
fn a_side_load_breaks_a_one_newton_metre_torque_limit() {
    // 10 kg whose center is 1 m off the anchor. Gravity is about 98 N of linear reaction and
    // about 98 N·m of moment. The angular rows can stay quiet; the lever arm is what breaks it.
    let mut app = physics_app();
    let anchor = boxed(
        &mut app,
        RigidBody::static_body(),
        Vec3::splat(0.1),
        Vec3::new(0.0, 5.0, 0.0),
    );
    let block = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(1.0, 5.0, 0.0)),
            RigidBody::dynamic(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::splat(0.2),
                },
                density: 0.0,
                ..Collider::default()
            },
            regia_physics::Mass::Explicit {
                mass: 10.0,
                center_of_mass: Vec3::ZERO,
                principal_inertia: Vec3::ONE,
            },
            Velocity::default(),
        ))
        .id();
    app.world_mut().entity_mut(anchor).insert((
        Joint {
            kind: JointKind::Fixed,
            connected: block,
            anchor: Vec3::ZERO,
            connected_anchor: Vec3::new(-1.0, 0.0, 0.0),
            break_force: 0.0,
        },
        JointBreakTorque(1.0),
    ));
    assert!(
        first_break(&mut app, 120).is_some(),
        "98 N·m of side load exceeds a 1 N·m torque limit"
    );
}

#[test]
fn without_a_torque_limit_or_under_it_the_lever_holds() {
    for torque in [0.0, 5.0, 1.0e9] {
        let mut app = physics_app();
        lever(&mut app, 0.0, torque);
        assert_eq!(first_break(&mut app, 120), None, "limit {torque}");
    }
}

#[test]
fn the_force_limit_still_works_beside_the_torque_limit() {
    let mut app = physics_app();
    // A 0.05 N linear limit is under the block's load (rapier writes back the last
    // substep's impulses, so the reported force is the true ≈0.63 N weight); no torque limit.
    lever(&mut app, 0.05, 0.0);
    assert!(first_break(&mut app, 120).is_some());
}

#[test]
fn a_force_limit_between_a_quarter_and_the_full_load_still_breaks() {
    // Regression (k28-physics-2): `break_force` was measured on the full step dt while rapier
    // writes back the last substep's impulses, so the reported linear force was under-reported
    // by `solver_iterations` (4 by default). A limit of 0.3 N is above a quarter of the block's
    // ≈0.63 N weight but below the true load: the buggy code reported ≈0.16 N and the joint
    // held; the fixed code reports the true ≈0.63 N and the joint breaks.
    let mut app = physics_app();
    lever(&mut app, 0.3, 0.0);
    assert!(
        first_break(&mut app, 120).is_some(),
        "a 0.3 N limit is under the ≈0.63 N weight but over a quarter of it; \
         the joint must break at the documented threshold"
    );
}

#[test]
fn debug_draw_off_collects_nothing_and_on_draws_contacts_joints_and_centers() {
    let mut app = physics_app();
    boxed(
        &mut app,
        RigidBody::static_body(),
        Vec3::new(10.0, 0.5, 10.0),
        Vec3::new(0.0, -0.5, 0.0),
    );
    boxed(
        &mut app,
        RigidBody::dynamic(),
        Vec3::splat(0.5),
        Vec3::new(0.0, 0.45, 0.0),
    );
    lever(&mut app, 0.0, 0.0);
    for _ in 0..30 {
        app.update();
    }
    {
        let draw = app.world().resource::<PhysicsDebugDraw>();
        assert!(!draw.is_on());
        assert_eq!(draw.stats().collections, 0, "off: the collection never ran");
        assert_eq!(draw.stats().lines_emitted, 0);
        assert!(draw.lines().is_empty());
    }
    app.world_mut().resource_mut::<PhysicsDebugDraw>().set_on(true);
    for _ in 0..3 {
        app.update();
    }
    let draw = app.world().resource::<PhysicsDebugDraw>();
    assert_eq!(draw.stats().collections, 3);
    let colors = |c: [f32; 3]| draw.lines().iter().filter(|l| l.color == c).count();
    assert!(
        colors(regia_physics::debug_draw::CONTACT_COLOR) >= 1,
        "the resting box touches the floor"
    );
    assert_eq!(
        colors(regia_physics::debug_draw::JOINT_COLOR),
        3,
        "one joint: two arms and the gap"
    );
    assert_eq!(
        colors(regia_physics::debug_draw::COM_COLOR),
        2 * 3,
        "two dynamic bodies, three arms each"
    );
}

#[test]
fn debug_draw_kinds_select_what_is_drawn_and_off_clears() {
    let mut app = physics_app();
    lever(&mut app, 0.0, 0.0);
    app.world_mut().resource_mut::<PhysicsDebugDraw>().set_on(true);
    let mut kinds = PhysicsDebugKinds::NONE;
    assert!(kinds.enable("joints"));
    assert!(!kinds.enable("nonsense"));
    app.world_mut()
        .resource_mut::<PhysicsDebugDraw>()
        .set_kinds(kinds);
    app.update();
    app.update();
    let draw = app.world().resource::<PhysicsDebugDraw>();
    assert_eq!(draw.lines().len(), 3);
    assert!(
        draw.lines()
            .iter()
            .all(|l| l.color == regia_physics::debug_draw::JOINT_COLOR)
    );
    app.world_mut().resource_mut::<PhysicsDebugDraw>().set_on(false);
    assert!(app.world().resource::<PhysicsDebugDraw>().lines().is_empty());
}

#[test]
fn a_collider_takes_its_named_material_and_the_default_material_changes_nothing() {
    // A scene with the default material on every collider is bit-identical to one without.
    let digest = |with_ref: bool| {
        let mut app = physics_app();
        let mut materials = PhysicsMaterials::default();
        materials.insert(PhysicsMaterial::default());
        app.insert_resource(materials);
        boxed(
            &mut app,
            RigidBody::static_body(),
            Vec3::new(10.0, 0.5, 10.0),
            Vec3::new(0.0, -0.5, 0.0),
        );
        let b = boxed(
            &mut app,
            RigidBody::dynamic(),
            Vec3::splat(0.5),
            Vec3::new(0.0, 2.0, 0.0),
        );
        app.world_mut().get_mut::<Velocity>(b).expect("velocity").linear = Vec3::new(2.0, 0.0, 1.0);
        if with_ref {
            app.world_mut()
                .entity_mut(b)
                .insert(PhysicsMaterialRef("default".into()));
        }
        for _ in 0..120 {
            app.update();
        }
        app.world().resource::<PhysicsWorld>().state_digest()
    };
    assert_eq!(digest(false), digest(true));

    let mut app = physics_app();
    let mut ice = PhysicsMaterial::new("ice");
    ice.friction = 0.02;
    ice.restitution = 0.3;
    ice.density = 2.5;
    ice.friction_combine = CombineRule::Min;
    app.world_mut().resource_mut::<PhysicsMaterials>().insert(ice);
    let named = boxed(&mut app, RigidBody::dynamic(), Vec3::splat(0.5), Vec3::ZERO);
    let plain = boxed(&mut app, RigidBody::dynamic(), Vec3::splat(0.5), Vec3::X * 4.0);
    app.world_mut()
        .entity_mut(named)
        .insert(PhysicsMaterialRef("ice".into()));
    app.world_mut()
        .entity_mut(plain)
        .insert(PhysicsMaterialRef("missing".into()));
    app.update();
    let c = app.world().get::<Collider>(named).expect("collider");
    assert_eq!(
        (c.friction, c.restitution, c.density, c.friction_combine),
        (0.02, 0.3, 2.5, CombineRule::Min)
    );
    assert_eq!(
        *app.world().get::<Collider>(plain).expect("collider"),
        Collider {
            shape: ColliderShape::Box {
                half_extents: Vec3::splat(0.5)
            },
            ..Collider::default()
        }
    );
    assert_eq!(app.world().resource::<PhysicsMaterials>().unresolved, 1);
}
