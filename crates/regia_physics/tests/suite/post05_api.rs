// SPDX-License-Identifier: MPL-2.0
//! Spec 15 physics API remainder (ADR-7233): the explicit teleport (velocity kept or cleared, no
//! contact from the jump, no velocity derived from it) and `closest_point`'s outward normal and
//! signed distance. Fixed 60 Hz steps from deterministic inputs; no wall-clock budgets.
//!
//! The analytic expectations (distance from a point to an axis-aligned box face, edge or a
//! sphere) are elementary geometry derived here.

use std::time::Duration;

use regia_app::{App, FixedTime, TimeUpdateStrategy};
use regia_core::math::Vec3;
use regia_ecs::ecs::message::{MessageCursor, Messages};
use regia_ecs::prelude::Entity;
use regia_physics::{
    ActiveEvents, Collider, ColliderShape, CollisionStarted, PhysicsPlugin, PhysicsWorld, QueryFilter,
    RigidBody, TeleportCommands, TeleportVelocity, Velocity, WorldQueries, teleport,
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

fn spawn_static(app: &mut App, shape: ColliderShape, at: Vec3) -> Entity {
    app.world_mut()
        .spawn((
            Transform::from_translation(at),
            RigidBody::static_body(),
            Collider {
                shape,
                active_events: ActiveEvents::ALL,
                ..Collider::default()
            },
        ))
        .id()
}

/// A weightless CCD ball (radius 0.25) at `at` moving at `velocity`.
fn floating_ball(app: &mut App, at: Vec3, velocity: Vec3) -> Entity {
    app.world_mut()
        .spawn((
            Transform::from_translation(at),
            RigidBody {
                gravity_scale: 0.0,
                ccd: true,
                can_sleep: false,
                ..RigidBody::dynamic()
            },
            Collider {
                shape: ColliderShape::Sphere { radius: 0.25 },
                active_events: ActiveEvents::ALL,
                ..Collider::default()
            },
            Velocity {
                linear: velocity,
                angular: Vec3::ZERO,
            },
        ))
        .id()
}

/// A thin wall (10 cm thick) across the x axis at x = 0.
fn thin_wall(app: &mut App) -> Entity {
    spawn_static(
        app,
        ColliderShape::Box {
            half_extents: Vec3::new(0.05, 2.0, 2.0),
        },
        Vec3::ZERO,
    )
}

fn run(app: &mut App, steps: usize, cursor: &mut MessageCursor<CollisionStarted>) -> Vec<CollisionStarted> {
    let mut seen = Vec::new();
    for _ in 0..steps {
        app.update();
        let messages = app.world().resource::<Messages<CollisionStarted>>();
        seen.extend(cursor.read(messages).copied());
    }
    seen
}

fn translation(app: &App, entity: Entity) -> Vec3 {
    app.world()
        .get::<Transform>(entity)
        .map_or(Vec3::NAN, |t| t.translation)
}

fn linear_velocity(app: &App, entity: Entity) -> Vec3 {
    app.world()
        .get::<Velocity>(entity)
        .map_or(Vec3::NAN, |v| v.linear)
}

/// Teleporting a CCD ball across a thin wall with `Keep`: the jump is not swept (no contact with
/// the wall), and the ball carries on at the velocity it had.
#[test]
fn a_kept_velocity_teleport_crosses_a_wall_without_a_contact() {
    let mut app = physics_app();
    let wall = thin_wall(&mut app);
    let ball = floating_ball(&mut app, Vec3::new(-2.0, 0.0, 0.0), Vec3::new(1.0, 0.0, 0.0));
    let mut cursor = MessageCursor::default();
    let before = run(&mut app, 10, &mut cursor);
    assert!(
        before.is_empty(),
        "nothing touches before the teleport: {before:?}"
    );

    assert!(
        teleport(
            app.world_mut(),
            ball,
            Transform::from_translation(Vec3::new(2.0, 0.0, 0.0)),
            TeleportVelocity::Keep,
        ),
        "a body teleports"
    );
    let after = run(&mut app, 30, &mut cursor);
    assert!(
        after.iter().all(|e| e.entity_a != wall && e.entity_b != wall),
        "the teleport swept nothing through the wall: {after:?}"
    );
    let x = translation(&app, ball).x;
    // 30 steps at 1 m/s from x = 2 (one step may land on the teleport frame): 2.48 .. 2.52.
    assert!(
        (x - 2.5).abs() < 0.03,
        "the ball moved on from the target at 1 m/s (x = {x})"
    );
    let v = linear_velocity(&app, ball);
    assert!((v - Vec3::X).length() < 1e-3, "velocity kept: {v:?}");
}

/// `Clear` stops the body where it lands.
#[test]
fn a_cleared_velocity_teleport_stops_the_body_at_the_target() {
    let mut app = physics_app();
    thin_wall(&mut app);
    let ball = floating_ball(&mut app, Vec3::new(-2.0, 0.0, 0.0), Vec3::new(1.0, 0.0, 0.0));
    let mut cursor = MessageCursor::default();
    run(&mut app, 10, &mut cursor);
    assert!(teleport(
        app.world_mut(),
        ball,
        Transform::from_translation(Vec3::new(2.0, 0.0, 0.0)),
        TeleportVelocity::Clear,
    ));
    let after = run(&mut app, 20, &mut cursor);
    assert!(after.is_empty(), "no contact from the jump: {after:?}");
    let at = translation(&app, ball);
    assert!(
        (at - Vec3::new(2.0, 0.0, 0.0)).length() < 1e-4,
        "the ball stays at the target: {at:?}"
    );
    let v = linear_velocity(&app, ball);
    assert!(v.length() < 1e-6, "velocity cleared: {v:?}");
}

/// The command form queues the same teleport.
#[test]
fn the_teleport_command_applies_on_flush() {
    let mut app = physics_app();
    let ball = floating_ball(&mut app, Vec3::new(-2.0, 0.0, 0.0), Vec3::new(0.0, 1.0, 0.0));
    let mut cursor = MessageCursor::default();
    run(&mut app, 5, &mut cursor);
    {
        let world = app.world_mut();
        world.commands().entity(ball).teleport(
            Transform::from_translation(Vec3::new(0.0, 0.0, 7.0)),
            TeleportVelocity::Clear,
        );
        world.flush();
    }
    run(&mut app, 5, &mut cursor);
    let at = translation(&app, ball);
    assert!(
        (at - Vec3::new(0.0, 0.0, 7.0)).length() < 1e-4,
        "commanded teleport landed: {at:?}"
    );
}

/// A kinematic body moved by its `Transform` gets the velocity of the move (it would hit what it
/// meets at that speed); teleported, it arrives with none.
#[test]
fn a_teleported_kinematic_body_gets_no_velocity_from_the_jump() {
    let mut app = physics_app();
    let moved = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::ZERO),
            RigidBody::kinematic(),
            Collider::default(),
            Velocity::default(),
        ))
        .id();
    let jumped = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, 0.0, 20.0)),
            RigidBody::kinematic(),
            Collider::default(),
            Velocity::default(),
        ))
        .id();
    let mut cursor = MessageCursor::default();
    run(&mut app, 5, &mut cursor);
    if let Some(mut t) = app.world_mut().get_mut::<Transform>(moved) {
        t.translation = Vec3::new(6.0, 0.0, 0.0);
    }
    assert!(teleport(
        app.world_mut(),
        jumped,
        Transform::from_translation(Vec3::new(6.0, 0.0, 20.0)),
        TeleportVelocity::Keep,
    ));
    run(&mut app, 1, &mut cursor);
    let control = linear_velocity(&app, moved).x;
    assert!(
        control > 100.0,
        "the control's move is a 6 m step at 60 Hz: {control}"
    );
    let v = linear_velocity(&app, jumped);
    assert!(v.length() < 1e-3, "no velocity from the teleport: {v:?}");
    let at = translation(&app, jumped);
    assert!((at - Vec3::new(6.0, 0.0, 20.0)).length() < 1e-4, "{at:?}");
}

/// Only bodies teleport: an entity without `RigidBody` (or a despawned one) is refused.
#[test]
fn teleport_refuses_an_entity_without_a_body() {
    let mut app = physics_app();
    let plain = app.world_mut().spawn(Transform::IDENTITY).id();
    assert!(!teleport(
        app.world_mut(),
        plain,
        Transform::from_translation(Vec3::X),
        TeleportVelocity::Keep
    ));
    assert_eq!(
        translation(&app, plain),
        Vec3::ZERO,
        "the refused teleport moved nothing"
    );
    app.world_mut().despawn(plain);
    assert!(!teleport(
        app.world_mut(),
        plain,
        Transform::IDENTITY,
        TeleportVelocity::Clear
    ));
}

// ---------------------------------------------------------------------------
// closest_point: normal and signed distance
// ---------------------------------------------------------------------------

/// A plate whose top face is y = 0 (x, z in -5..5) and a unit sphere at (20, 0, 0).
fn query_world() -> (App, Entity, Entity) {
    let mut app = physics_app();
    let plate = spawn_static(
        &mut app,
        ColliderShape::Box {
            half_extents: Vec3::new(5.0, 0.5, 5.0),
        },
        Vec3::new(0.0, -0.5, 0.0),
    );
    let ball = spawn_static(
        &mut app,
        ColliderShape::Sphere { radius: 1.0 },
        Vec3::new(20.0, 0.0, 0.0),
    );
    for _ in 0..6 {
        app.update();
    }
    (app, plate, ball)
}

fn near(a: Vec3, b: Vec3) -> bool {
    (a - b).length() < 1e-4
}

#[test]
fn closest_point_reports_the_outward_normal_and_signed_distance() {
    let (app, plate, ball) = query_world();
    let q = WorldQueries {
        world: app.world().resource::<PhysicsWorld>(),
        layers: None,
    };
    let cases = [
        // (query, entity, surface point, normal, signed distance)
        (Vec3::new(0.0, 3.0, 0.0), plate, Vec3::ZERO, Vec3::Y, 3.0),
        // Off the plate's edge: the nearest feature is the top edge at x = 5.
        (
            Vec3::new(6.0, 1.0, 0.0),
            plate,
            Vec3::new(5.0, 0.0, 0.0),
            Vec3::new(1.0, 1.0, 0.0).normalize(),
            2.0f32.sqrt(),
        ),
        // Inside the plate, 0.1 m under the top face: the top is the nearest face.
        (
            Vec3::new(1.0, -0.1, 2.0),
            plate,
            Vec3::new(1.0, 0.0, 2.0),
            Vec3::Y,
            -0.1,
        ),
        // On the top face itself: distance zero, the face normal.
        (
            Vec3::new(-2.0, 0.0, 1.0),
            plate,
            Vec3::new(-2.0, 0.0, 1.0),
            Vec3::Y,
            0.0,
        ),
        (
            Vec3::new(20.0, 0.0, 3.0),
            ball,
            Vec3::new(20.0, 0.0, 1.0),
            Vec3::Z,
            2.0,
        ),
        (
            Vec3::new(20.0, 0.0, 0.5),
            ball,
            Vec3::new(20.0, 0.0, 1.0),
            Vec3::Z,
            -0.5,
        ),
    ];
    for (query, entity, surface, normal, distance) in cases {
        let hit = q
            .closest_point(query, 100.0, QueryFilter::new())
            .unwrap_or_else(|| panic!("a hit for {query:?}"));
        assert_eq!(hit.entity, entity, "{query:?}");
        assert!(
            near(hit.normal, normal),
            "{query:?}: normal {:?}, expected {normal:?}",
            hit.normal
        );
        assert!(
            (hit.distance - distance).abs() < 1e-4,
            "{query:?}: distance {}, expected {distance}",
            hit.distance
        );
        if distance != 0.0 {
            // On the boundary `is_inside` is parry's call (inside-or-on); off it, the sign decides.
            assert_eq!(hit.is_inside, distance < 0.0, "{query:?}");
        }
        // The documented invariant: the surface point is `query - normal * distance`.
        assert!(
            near(query - hit.normal * hit.distance, surface),
            "{query:?}: surface {:?}",
            query - hit.normal * hit.distance
        );
        // `point` keeps its meaning: the query point itself when inside, else the surface point.
        let expected_point = if hit.is_inside { query } else { surface };
        assert!(
            near(hit.point, expected_point),
            "{query:?}: point {:?}",
            hit.point
        );
        assert!((hit.normal.length() - 1.0).abs() < 1e-5, "unit normal");
    }
}

/// A static triangle mesh at the origin: triangle 0 is a floor in y = 0 (x, z in -2..2, normal +Y),
/// triangle 1 a wall in x = 3 (y 0..4, z -2..2, normal +X). Review rev-claude-33-k finding 3: on a
/// mesh the on-surface normal must come from the triangle the point lies on, not triangle 0.
fn mesh_world() -> (App, Entity) {
    let mut app = physics_app();
    let mesh = spawn_static(
        &mut app,
        ColliderShape::TriMesh {
            vertices: vec![
                Vec3::new(-2.0, 0.0, -2.0),
                Vec3::new(-2.0, 0.0, 2.0),
                Vec3::new(2.0, 0.0, -2.0),
                Vec3::new(3.0, 0.0, -2.0),
                Vec3::new(3.0, 4.0, -2.0),
                Vec3::new(3.0, 0.0, 2.0),
            ],
            indices: vec![[0, 1, 2], [3, 4, 5]],
        },
        Vec3::ZERO,
    );
    for _ in 0..6 {
        app.update();
    }
    (app, mesh)
}

#[test]
fn closest_point_on_a_mesh_takes_the_normal_of_the_triangle_under_the_point() {
    let (app, mesh) = mesh_world();
    let q = WorldQueries {
        world: app.world().resource::<PhysicsWorld>(),
        layers: None,
    };
    for (query, normal, distance) in [
        // On the wall (triangle 1) itself: its face normal, distance zero.
        (Vec3::new(3.0, 1.0, 0.0), Vec3::X, 0.0),
        // On the floor (triangle 0) itself.
        (Vec3::new(-1.0, 0.0, -1.0), Vec3::Y, 0.0),
        // Off the wall: the point-to-surface direction.
        (Vec3::new(3.5, 1.0, 0.0), Vec3::X, 0.5),
    ] {
        let hit = q
            .closest_point(query, 100.0, QueryFilter::new())
            .unwrap_or_else(|| panic!("a hit for {query:?}"));
        assert_eq!(hit.entity, mesh, "{query:?}");
        assert!(
            near(hit.normal, normal),
            "{query:?}: normal {:?}, expected {normal:?}",
            hit.normal
        );
        assert!(
            (hit.distance - distance).abs() < 1e-4,
            "{query:?}: distance {}, expected {distance}",
            hit.distance
        );
    }
}
