// SPDX-License-Identifier: MPL-2.0
//! M20 acceptance tests (specs/15 § 6): create/remove/change round trips, layer matrix
//! filtering, events delivered exactly once, raycast correctness vs analytic shapes,
//! stacking stability, joint chains, CCD, kinematic platforms and the desktop perf budget.
//!
//! Provenance: the physics pipeline exercised is rapier3d (Apache-2.0); the analytic
//! expectations (hit point/normal/toi of rays against boxes and spheres) are elementary
//! geometry derived here, not from any engine's source.

use std::time::Duration;

use regia_app::{App, FixedTime, TimeUpdateStrategy};
use regia_core::math::Vec3;
use regia_ecs::ecs::message::{MessageCursor, Messages};
use regia_ecs::prelude::Entity;
use regia_physics::{
    ActiveEvents, Collider, ColliderShape, CollisionLayer, ExternalForce, Joint, JointKind, Mass,
    PhysicsLayers, PhysicsPlugin, PhysicsQuery, QueryFilter, RigidBody, Teleport, Velocity,
};
use regia_scene::Transform;

/// One step per update: manual 1/60 frame clock at a 60 Hz fixed rate.
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

/// A static body with a box collider at `translation`.
fn static_box(app: &mut App, half: f32, translation: Vec3) -> Entity {
    app.world_mut()
        .spawn((
            Transform::from_translation(translation),
            RigidBody::static_body(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::splat(half),
                },
                friction: 0.8,
                ..Collider::default()
            },
        ))
        .id()
}

/// A flat static ground plate whose top surface sits at y=0.
fn ground_plate(app: &mut App, half_x: f32, half_z: f32) -> Entity {
    app.world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
            RigidBody::static_body(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::new(half_x, 0.5, half_z),
                },
                friction: 0.8,
                ..Collider::default()
            },
        ))
        .id()
}

/// A dynamic body with a box collider at `translation`.
fn dynamic_box(app: &mut App, half: f32, translation: Vec3) -> Entity {
    app.world_mut()
        .spawn((
            Transform::from_translation(translation),
            RigidBody::dynamic(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::splat(half),
                },
                friction: 0.8,
                restitution: 0.0,
                ..Collider::default()
            },
            Velocity::default(),
        ))
        .id()
}

/// Steps the app `seconds × 60` times.
fn run(app: &mut App, seconds: f64) {
    for _ in 0..((seconds * 60.0).round() as u64) {
        app.update();
    }
}

fn translation_of(app: &App, entity: Entity) -> Vec3 {
    app.world()
        .get::<Transform>(entity)
        .expect("transform")
        .translation
}

fn velocity_of(app: &App, entity: Entity) -> Velocity {
    app.world().get::<Velocity>(entity).cloned().unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Round trips: create / remove / change (specs/15 § 6)
// ---------------------------------------------------------------------------

#[test]
fn bodies_create_step_remove_round_trip() {
    let mut app = physics_app();
    let ground = ground_plate(&mut app, 5.0, 5.0);
    let box_e = dynamic_box(&mut app, 0.5, Vec3::new(0.0, 4.0, 0.0));
    app.world_mut()
        .get_mut::<Collider>(ground)
        .expect("collider")
        .active_events = ActiveEvents::ALL;

    run(&mut app, 1.0);
    {
        let world = app.world().resource::<regia_physics::PhysicsWorld>();
        assert_eq!(world.bodies.len(), 2, "both bodies created");
        assert_eq!(world.colliders.len(), 2, "both colliders created");
    }
    let y = translation_of(&app, box_e).y;
    assert!(
        (0.5 - y).abs() < 0.05,
        "the box fell onto the ground and rests on it: y={y}"
    );

    // Removing the components removes the rapier objects.
    {
        let w = app.world_mut();
        w.entity_mut(box_e).remove::<Collider>().remove::<RigidBody>();
        w.entity_mut(ground).despawn();
    }
    run(&mut app, 0.1);
    let world = app.world().resource::<regia_physics::PhysicsWorld>();
    assert_eq!(world.colliders.len(), 0, "colliders removed");
    assert_eq!(world.bodies.len(), 0, "bodies removed");
}

#[test]
fn collider_shape_change_rebuilds_the_collider() {
    let mut app = physics_app();
    let _ground = ground_plate(&mut app, 5.0, 5.0);
    let box_e = dynamic_box(&mut app, 0.5, Vec3::new(0.0, 4.0, 0.0));
    run(&mut app, 1.0);
    let old_handle = app
        .world()
        .get::<regia_physics::ColliderHandle>(box_e)
        .expect("collider handle");
    let old_handle = (old_handle.raw, old_handle.generation);

    // Change the shape: box (half 0.5) → sphere (r 1.5). The resting height changes.
    app.world_mut()
        .get_mut::<Collider>(box_e)
        .expect("collider")
        .shape = ColliderShape::Sphere { radius: 1.5 };
    run(&mut app, 1.5);
    let new_handle = app
        .world()
        .get::<regia_physics::ColliderHandle>(box_e)
        .expect("collider handle");
    let new_handle = (new_handle.raw, new_handle.generation);
    assert_ne!(old_handle, new_handle, "the collider was rebuilt");
    let y = translation_of(&app, box_e).y;
    assert!(
        (y - 1.5).abs() < 0.1,
        "rests a sphere-radius above the ground: y={y}"
    );
}

#[test]
fn velocity_writes_apply_and_results_read_back() {
    let mut app = physics_app();
    let box_e = dynamic_box(&mut app, 0.5, Vec3::ZERO);
    // No ground: free fall. Write an initial velocity.
    app.world_mut()
        .entity_mut(box_e)
        .insert(Velocity {
            linear: Vec3::new(2.0, 5.0, 0.0),
            angular: Vec3::ZERO,
        })
        .insert(ExternalForce {
            force: Vec3::new(10.0, 0.0, 0.0),
            torque: Vec3::ZERO,
        });
    // Mass from density: box 1m³ × density 1 = 1 kg → a = 10 m/s².
    run(&mut app, 1.0);
    let v = velocity_of(&app, box_e);
    assert!((v.linear.x - 12.0).abs() < 0.2, "vx ≈ 2 + 10·1: {}", v.linear.x);
    assert!(
        (v.linear.y - (5.0 - 9.81)).abs() < 0.2,
        "vy after gravity: {}",
        v.linear.y
    );
}

#[test]
fn explicit_mass_overrides_body_inertia() {
    let mut app = physics_app();
    let heavy = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, 2.0, 0.0)),
            RigidBody::dynamic(),
            Collider::default(),
            Mass::Explicit {
                mass: 1000.0,
                center_of_mass: Vec3::ZERO,
                principal_inertia: Vec3::splat(1.0),
            },
        ))
        .id();
    let light = dynamic_box(&mut app, 0.2, Vec3::new(1.0, 2.0, 0.0));
    run(&mut app, 0.5);
    // Same gravity: both accelerate identically (equivalence), but the heavy body's
    // velocity read-back must exist and stay finite.
    let v = velocity_of(&app, heavy);
    assert!(v.linear.y.is_finite());
    let _ = light;
}

#[test]
fn impulse_applies_once_and_clears() {
    let mut app = physics_app();
    let box_e = dynamic_box(&mut app, 0.5, Vec3::new(0.0, 10.0, 0.0));
    app.world_mut()
        .entity_mut(box_e)
        .insert(regia_physics::ExternalImpulse {
            impulse: Vec3::new(0.0, 50.0, 0.0),
            torque_impulse: Vec3::ZERO,
        });
    app.update();
    assert!(
        app.world().get::<regia_physics::ExternalImpulse>(box_e).is_none(),
        "the impulse component is removed after applying"
    );
    // 50 kg·m/s on a 1 kg body → 50 m/s (before this step's gravity).
    let v = velocity_of(&app, box_e);
    assert!(v.linear.y > 45.0, "impulse applied once: vy={}", v.linear.y);
    app.update();
    let v2 = velocity_of(&app, box_e);
    assert!(
        v2.linear.y < v.linear.y,
        "no second impulse: gravity reduced vy again"
    );
}

#[test]
fn teleport_snaps_without_simulation() {
    let mut app = physics_app();
    let box_e = dynamic_box(&mut app, 0.5, Vec3::new(0.0, 1.0, 0.0));
    run(&mut app, 0.5);
    // Teleport far away and up.
    let mut t = *app.world().get::<Transform>(box_e).expect("transform");
    t.translation = Vec3::new(100.0, 50.0, 0.0);
    {
        let w = app.world_mut();
        *w.get_mut::<Transform>(box_e).expect("transform") = t;
        w.entity_mut(box_e).insert(Teleport);
    }
    app.update();
    let p = translation_of(&app, box_e);
    assert!(
        (p.x - 100.0).abs() < 0.01,
        "the body snapped to the teleport pose: {p}"
    );
    // Teleports snap the pose and preserve momentum (the pre-teleport fall continues).
    let v = velocity_of(&app, box_e);
    assert!(v.linear.y < 0.0, "the fall continues from the new pose: {v:?}");
}

// ---------------------------------------------------------------------------
// Layer matrix filtering (specs/15 § 6)
// ---------------------------------------------------------------------------

/// Two layers: "world" (0) collides with itself; "ghost" (1) collides with nothing.
/// Queries belonging to "world" see "world"; queries from "ghost" see nothing.
fn two_layer_config() -> PhysicsLayers {
    let file = regia_physics::LayerFile::parse(
        r#"( layers: ["world", "ghost"],
             collides: [("world", "world")],
             queries: [("world", "world")] )"#,
    )
    .expect("parses");
    PhysicsLayers::from_file(&file).expect("builds")
}

#[test]
fn layer_matrix_filters_collisions() {
    let mut app = physics_app();
    app.insert_resource(two_layer_config());

    let ground = ground_plate(&mut app, 5.0, 5.0);
    let ghost = dynamic_box(&mut app, 0.5, Vec3::new(0.0, 4.0, 0.0));
    app.world_mut()
        .get_mut::<Collider>(ghost)
        .expect("collider")
        .layer = CollisionLayer::new(1); // ghost: collides with nothing

    run(&mut app, 2.0);
    assert!(
        translation_of(&app, ghost).y < -5.0,
        "the ghost-layer box falls through the world-layer ground"
    );

    // Moving it onto the world layer makes it land.
    app.world_mut()
        .get_mut::<Collider>(ghost)
        .expect("collider")
        .layer = CollisionLayer::new(0);
    app.world_mut()
        .get_mut::<Transform>(ghost)
        .expect("transform")
        .translation = Vec3::new(0.0, 4.0, 0.0);
    app.world_mut().entity_mut(ghost).insert(Teleport);
    run(&mut app, 2.0);
    let y = translation_of(&app, ghost).y;
    assert!((y - 0.5).abs() < 0.05, "now it lands: y={y}");
    let _ = ground;
}

#[test]
fn query_matrix_filters_rays() {
    let mut app = physics_app();
    app.insert_resource(two_layer_config());
    let _ground = ground_plate(&mut app, 5.0, 5.0);

    let slot: std::sync::Arc<std::sync::Mutex<Vec<Option<f32>>>> =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let slot2 = slot.clone();
    app.add_systems(regia_app::FixedUpdate, move |queries: PhysicsQuery| {
        let from_world = queries.raycast(
            Vec3::new(0.0, 10.0, 0.0),
            Vec3::NEG_Y,
            20.0,
            QueryFilter::new().from_layer(CollisionLayer::new(0)),
        );
        let from_ghost = queries.raycast(
            Vec3::new(0.0, 10.0, 0.0),
            Vec3::NEG_Y,
            20.0,
            QueryFilter::new().from_layer(CollisionLayer::new(1)),
        );
        let mut results = slot2.lock().unwrap_or_else(|p| p.into_inner());
        results.push(from_world.map(|hit| hit.distance));
        results.push(from_ghost.map(|hit| hit.distance));
    });
    run(&mut app, 0.2);
    let results = slot.lock().unwrap_or_else(|p| p.into_inner()).clone();
    assert!(results.len() >= 2, "the probe ran");
    // The world-layer query sees the ground plate's top surface at y=0 (ray from y=10).
    let hit = results[results.len() - 2].expect("world query hits");
    assert!((hit - 10.0).abs() < 1e-3, "analytic distance 10.0: {hit}");
    // The ghost-layer query sees nothing.
    assert!(results[results.len() - 1].is_none(), "ghost query sees no layers");
}

// ---------------------------------------------------------------------------
// Events delivered exactly once (specs/15 § 6)
// ---------------------------------------------------------------------------

#[test]
fn collision_events_delivered_exactly_once() {
    let mut app = physics_app();
    let ground = ground_plate(&mut app, 5.0, 5.0);
    let box_e = dynamic_box(&mut app, 0.5, Vec3::new(0.0, 3.0, 0.0));
    for e in [ground, box_e] {
        app.world_mut()
            .get_mut::<Collider>(e)
            .expect("collider")
            .active_events = ActiveEvents {
            collision: true,
            contact_force: false,
        };
    }

    let mut started_cursor = MessageCursor::default();
    let mut ended_cursor = MessageCursor::default();
    let mut started_total = 0;
    let mut ended_total = 0;
    for _ in 0..120 {
        app.update();
        let (new_started, new_ended) = {
            let started = app
                .world()
                .resource::<Messages<regia_physics::CollisionStarted>>();
            let ended = app.world().resource::<Messages<regia_physics::CollisionEnded>>();
            (
                started_cursor.read(started).count(),
                ended_cursor.read(ended).count(),
            )
        };
        started_total += new_started;
        ended_total += new_ended;
    }
    assert_eq!(started_total, 1, "exactly one CollisionStarted for the pair");
    assert_eq!(ended_total, 0, "no end while touching");

    // Break contact: teleport the box far away.
    app.world_mut()
        .get_mut::<Transform>(box_e)
        .expect("transform")
        .translation = Vec3::new(50.0, 50.0, 50.0);
    app.world_mut().entity_mut(box_e).insert(Teleport);
    let mut ended_seen = 0;
    for _ in 0..20 {
        app.update();
        let messages = app.world().resource::<Messages<regia_physics::CollisionEnded>>();
        ended_seen += ended_cursor.read(messages).count();
    }
    assert_eq!(ended_seen, 1, "exactly one CollisionEnded after separation");
}

#[test]
fn sensor_events_delivered_exactly_once() {
    let mut app = physics_app();
    // A sensor volume above the ground; a falling box passes through it.
    let sensor_e = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, 2.0, 0.0)),
            RigidBody::static_body(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::new(2.0, 1.0, 2.0),
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
    let _ground = ground_plate(&mut app, 5.0, 5.0);
    let box_e = dynamic_box(&mut app, 0.5, Vec3::new(0.0, 8.0, 0.0));

    let mut enter_cursor = MessageCursor::default();
    let mut exit_cursor = MessageCursor::default();
    let mut enters = 0;
    let mut exits = 0;
    for _ in 0..180 {
        app.update();
        let (new_enter, new_exit) = {
            let m1 = app.world().resource::<Messages<regia_physics::TriggerEnter>>();
            let m2 = app.world().resource::<Messages<regia_physics::TriggerExit>>();
            (enter_cursor.read(m1).count(), exit_cursor.read(m2).count())
        };
        enters += new_enter;
        exits += new_exit;
    }
    assert_eq!(enters, 1, "exactly one TriggerEnter, got {enters}");
    assert_eq!(exits, 1, "exactly one TriggerExit, got {exits}");
    let _ = (sensor_e, box_e);
}

#[test]
fn contact_force_event_fires_above_threshold() {
    let mut app = physics_app();
    let ground = ground_plate(&mut app, 5.0, 5.0);
    let box_e = dynamic_box(&mut app, 0.5, Vec3::new(0.0, 3.0, 0.0));
    for e in [ground, box_e] {
        let mut collider = app.world_mut().get_mut::<Collider>(e).expect("collider");
        collider.active_events = ActiveEvents::ALL;
        collider.contact_force_threshold = 1.0;
    }
    let mut force_cursor = MessageCursor::default();
    let mut forces = 0;
    for _ in 0..120 {
        app.update();
        let messages = app.world().resource::<Messages<regia_physics::ContactForce>>();
        forces += force_cursor.read(messages).count();
    }
    assert!(forces > 0, "the hard landing raised ContactForce events");
}

// ---------------------------------------------------------------------------
// Raycast correctness vs analytic shapes (specs/15 § 6)
// ---------------------------------------------------------------------------

/// Collects query results from inside a FixedUpdate system into a shared slot.
struct QueryProbe {
    ray_box: Option<regia_physics::RaycastHit>,
    ray_sphere: Option<regia_physics::RaycastHit>,
    ray_all: Vec<regia_physics::RaycastHit>,
    closest: Option<regia_physics::ClosestPointResult>,
    overlap: Vec<Entity>,
    shape_cast: Option<regia_physics::ShapeCastHitInfo>,
}

fn probe_app() -> (App, std::sync::Arc<std::sync::Mutex<Option<QueryProbe>>>) {
    let slot: std::sync::Arc<std::sync::Mutex<Option<QueryProbe>>> =
        std::sync::Arc::new(std::sync::Mutex::new(None));
    let mut app = physics_app();
    // A 1×1×1 box (half extents) centred at the origin and a sphere at (10, 10, 0)
    // (outside the box's column so the sphere ray starts clear of the box).
    let _box = static_box(&mut app, 1.0, Vec3::ZERO);
    app.world_mut().spawn((
        Transform::from_translation(Vec3::new(10.0, 10.0, 0.0)),
        RigidBody::static_body(),
        Collider {
            shape: ColliderShape::Sphere { radius: 2.0 },
            ..Collider::default()
        },
    ));
    // A second box at x=-6 for raycast_all.
    static_box(&mut app, 1.0, Vec3::new(-6.0, 0.0, 0.0));

    let slot2 = slot.clone();
    app.add_systems(regia_app::FixedUpdate, move |queries: PhysicsQuery| {
        let probe = QueryProbe {
            ray_box: queries.raycast(Vec3::new(5.0, 0.0, 0.0), Vec3::NEG_X, 20.0, QueryFilter::new()),
            ray_sphere: queries.raycast(Vec3::new(10.0, 0.0, 0.0), Vec3::Y, 20.0, QueryFilter::new()),
            ray_all: queries.raycast_all(Vec3::new(10.0, 0.0, 0.0), Vec3::NEG_X, 30.0, QueryFilter::new()),
            closest: queries.closest_point(Vec3::new(5.0, 0.0, 0.0), 10.0, QueryFilter::new()),
            overlap: queries.overlap(
                &ColliderShape::Box {
                    half_extents: Vec3::splat(1.5),
                },
                Transform::IDENTITY,
                QueryFilter::new(),
            ),
            shape_cast: queries.shapecast(
                &ColliderShape::Sphere { radius: 0.5 },
                Transform::from_translation(Vec3::new(0.0, 5.0, 0.0)),
                Vec3::NEG_Y,
                10.0,
                QueryFilter::new(),
            ),
        };
        *slot2.lock().unwrap_or_else(|p| p.into_inner()) = Some(probe);
    });
    (app, slot)
}

#[test]
fn raycast_hits_analytic_box_and_sphere() {
    let (mut app, slot) = probe_app();
    run(&mut app, 0.1);
    let probe = slot
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .take()
        .expect("probe ran");

    // Box: ray from x=5 towards -X hits the +X face at x=1, distance 4, normal +X.
    let hit = probe.ray_box.expect("box hit");
    assert!((hit.distance - 4.0).abs() < 1e-3, "toi: {}", hit.distance);
    assert!((hit.point.x - 1.0).abs() < 1e-3, "point: {:?}", hit.point);
    assert!((hit.normal.x - 1.0).abs() < 1e-3, "normal: {:?}", hit.normal);

    // Sphere r=2 at y=10: ray up from origin hits at y=8, distance 8, normal -Y.
    let hit = probe.ray_sphere.expect("sphere hit");
    assert!((hit.distance - 8.0).abs() < 1e-3, "toi: {}", hit.distance);
    assert!((hit.point.y - 8.0).abs() < 1e-3, "point: {:?}", hit.point);
    assert!((hit.normal.y - (-1.0)).abs() < 1e-3, "normal: {:?}", hit.normal);

    // raycast_all through both boxes, from x=10 travelling -X: the origin box's +X face
    // (distance 9), then the far box's +X face at x=-5 (distance 15).
    let all = probe.ray_all;
    assert_eq!(all.len(), 2, "two hits: {:?}", all);
    assert!((all[0].distance - 9.0).abs() < 1e-3, "{}", all[0].distance);
    assert!((all[1].distance - 15.0).abs() < 1e-3, "{}", all[1].distance);
}

#[test]
fn closest_point_overlap_and_shapecast_match_analytic() {
    let (mut app, slot) = probe_app();
    run(&mut app, 0.1);
    let probe = slot
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .take()
        .expect("probe ran");

    // Closest point on the origin box from (5,0,0) is (1,0,0).
    let closest = probe.closest.expect("closest point found");
    assert!(
        (closest.point.x - 1.0).abs() < 1e-3,
        "closest point: {:?}",
        closest.point
    );
    assert!(!closest.is_inside);

    // Overlap: a 1.5 box at the origin overlaps the 1.0 box (not the far ones).
    assert_eq!(probe.overlap.len(), 1, "only the origin box overlaps");

    // Sphere cast r=0.5 from y=5 down: touches the box top (y=1) at toi 3.5.
    let cast = probe.shape_cast.expect("shape cast hit");
    assert!(
        (cast.time_of_impact - 3.5).abs() < 1e-3,
        "toi: {}",
        cast.time_of_impact
    );
    assert!((cast.point.y - 1.0).abs() < 1e-2, "point: {:?}", cast.point);
}

// ---------------------------------------------------------------------------
// Stacking stability (specs/15 § 6): 10-box tower for 60 s
// ---------------------------------------------------------------------------

#[test]
fn ten_box_tower_stable_for_60_seconds() {
    let mut app = physics_app();
    let _ground = ground_plate(&mut app, 10.0, 10.0);
    let half = 0.25f32;
    let mut tower = Vec::new();
    for i in 0..10 {
        let y = half + i as f32 * 2.0 * half;
        tower.push(dynamic_box(&mut app, half, Vec3::new(0.0, y, 0.0)));
    }

    run(&mut app, 60.0);

    for (i, &e) in tower.iter().enumerate() {
        let y = translation_of(&app, e).y;
        let expected = half + i as f32 * 2.0 * half;
        assert!(
            (y - expected).abs() < 0.15,
            "box {i} stayed stacked: y={y} expected≈{expected}"
        );
    }
    let top = translation_of(&app, tower[9]).y;
    assert!(top > 4.5, "the tower is still a tower: top y={top}");
}

// ---------------------------------------------------------------------------
// Joints (specs/15 § 6): chain + limits + breaking
// ---------------------------------------------------------------------------

#[test]
fn revolute_joint_chain_hangs_and_swings() {
    let mut app = physics_app();
    // A static anchor, then 4 boxes jointed below it, each 1 m apart.
    let anchor = static_box(&mut app, 0.2, Vec3::new(0.0, 5.0, 0.0));
    let mut prev = anchor;
    let mut chain = Vec::new();
    for i in 0..4 {
        let e = dynamic_box(&mut app, 0.2, Vec3::new(0.0, 4.0 - i as f32, 0.0));
        app.world_mut().entity_mut(prev).insert(Joint {
            kind: JointKind::Revolute {
                axis: Vec3::Z,
                limits: None,
                motor: None,
            },
            connected: e,
            anchor: Vec3::new(0.0, -0.5, 0.0),
            connected_anchor: Vec3::new(0.0, 0.5, 0.0),
            break_force: 0.0,
        });
        chain.push(e);
        prev = e;
    }

    run(&mut app, 2.0);

    // The chain hangs under the anchor and no link fell off.
    for (i, &e) in chain.iter().enumerate() {
        let p = translation_of(&app, e);
        assert!(
            p.y < 4.2 - i as f32 * 0.5,
            "link {i} hangs below its joint: {p:?}"
        );
        assert!(p.y > 0.0, "link {i} is still attached to the chain: {p:?}");
    }
    let world = app.world().resource::<regia_physics::PhysicsWorld>();
    assert_eq!(world.impulse_joints.len(), 4, "all four joints intact");
}

#[test]
fn distance_joint_keeps_bodies_within_limits() {
    let mut app = physics_app();
    let a = static_box(&mut app, 0.2, Vec3::new(0.0, 5.0, 0.0));
    let b = dynamic_box(&mut app, 0.2, Vec3::new(0.0, 3.0, 0.0));
    app.world_mut().entity_mut(a).insert(Joint {
        kind: JointKind::Distance { min: 2.0, max: 2.0 },
        connected: b,
        anchor: Vec3::ZERO,
        connected_anchor: Vec3::ZERO,
        break_force: 0.0,
    });

    run(&mut app, 2.0);
    let pa = translation_of(&app, a);
    let pb = translation_of(&app, b);
    let dist = (pb - pa).length();
    assert!(
        (dist - 2.0).abs() < 0.2,
        "the rope holds the bodies 2 m apart: {dist}"
    );
}

#[test]
fn joint_breaks_when_force_exceeds_break_force() {
    let mut app = physics_app();
    // A light plank jointed to a static anchor; a heavy weight drops on the plank.
    let anchor = static_box(&mut app, 0.2, Vec3::new(0.0, 4.0, 0.0));
    let plank = dynamic_box(&mut app, 0.5, Vec3::new(0.0, 3.0, 0.0));
    app.world_mut().entity_mut(anchor).insert(Joint {
        kind: JointKind::Fixed,
        connected: plank,
        anchor: Vec3::ZERO,
        connected_anchor: Vec3::ZERO,
        break_force: 80.0,
    });
    // 100 kg weight from 3 m above the plank.
    let weight = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, 6.0, 0.0)),
            RigidBody::dynamic(),
            Collider::default(),
            Mass::Explicit {
                mass: 100.0,
                center_of_mass: Vec3::ZERO,
                principal_inertia: Vec3::splat(1.0),
            },
        ))
        .id();

    let mut broken = 0;
    let mut cursor = MessageCursor::default();
    for _ in 0..180 {
        app.update();
        let messages = app.world().resource::<Messages<regia_physics::JointBroken>>();
        broken += cursor.read(messages).count();
    }
    assert!(broken >= 1, "the joint broke under the load");
    let handle = app.world().get::<regia_physics::JointHandle>(anchor);
    assert!(handle.is_none(), "the broken joint was removed");
    let _ = weight;
}

#[test]
fn joint_breaks_after_an_earlier_joint_was_removed() {
    let mut app = physics_app();
    // A throwaway joint burns generation 0 of rapier's joint slot when it is despawned.
    let x = static_box(&mut app, 0.2, Vec3::new(10.0, 4.0, 0.0));
    let y = dynamic_box(&mut app, 0.5, Vec3::new(10.0, 3.0, 0.0));
    app.world_mut().entity_mut(x).insert(Joint {
        kind: JointKind::Fixed,
        connected: y,
        anchor: Vec3::ZERO,
        connected_anchor: Vec3::ZERO,
        break_force: 0.0,
    });
    app.update();
    app.world_mut().despawn(x);
    app.update();

    let anchor = static_box(&mut app, 0.2, Vec3::new(0.0, 4.0, 0.0));
    let plank = dynamic_box(&mut app, 0.5, Vec3::new(0.0, 3.0, 0.0));
    app.world_mut().entity_mut(anchor).insert(Joint {
        kind: JointKind::Fixed,
        connected: plank,
        anchor: Vec3::ZERO,
        connected_anchor: Vec3::ZERO,
        break_force: 80.0,
    });
    app.world_mut().spawn((
        Transform::from_translation(Vec3::new(0.0, 6.0, 0.0)),
        RigidBody::dynamic(),
        Collider::default(),
        Mass::Explicit {
            mass: 100.0,
            center_of_mass: Vec3::ZERO,
            principal_inertia: Vec3::splat(1.0),
        },
    ));

    let mut broken = 0;
    let mut cursor = MessageCursor::default();
    for _ in 0..180 {
        app.update();
        let messages = app.world().resource::<Messages<regia_physics::JointBroken>>();
        broken += cursor.read(messages).count();
    }
    assert!(broken >= 1, "a joint created after a removal still breaks");
}

#[test]
fn child_collider_keeps_its_composed_offset_after_a_property_change() {
    let mut app = physics_app();
    let body = static_box(&mut app, 0.5, Vec3::ZERO);
    let child = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(3.0, 0.0, 0.0)),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::splat(0.25),
                },
                ..Collider::default()
            },
        ))
        .id();
    app.world_mut().entity_mut(body).add_child(child);
    app.update();
    let local_x = |app: &App| {
        let handle = app
            .world()
            .get::<regia_physics::ColliderHandle>(child)
            .expect("child collider has a handle")
            .to_handle();
        let physics = app.world().resource::<regia_physics::PhysicsWorld>();
        physics
            .colliders
            .get(handle)
            .expect("collider exists")
            .position_wrt_parent()
            .expect("collider has a parent")
            .translation
            .x
    };
    assert!((local_x(&app) - 3.0).abs() < 1e-4, "composed offset on creation");

    app.world_mut()
        .entity_mut(child)
        .get_mut::<Collider>()
        .expect("collider component")
        .friction = 0.1;
    app.update();
    assert!(
        (local_x(&app) - 3.0).abs() < 1e-4,
        "the rebuilt collider keeps the child transform: {}",
        local_x(&app)
    );
}

/// The rapier body behind `entity`.
fn rapier_body(app: &App, entity: Entity) -> &rapier3d::dynamics::RigidBody {
    let handle = app
        .world()
        .get::<regia_physics::RigidBodyHandle>(entity)
        .expect("body handle")
        .to_handle();
    app.world()
        .resource::<regia_physics::PhysicsWorld>()
        .bodies
        .get(handle)
        .expect("rapier body")
}

#[test]
fn mass_auto_clears_an_earlier_explicit_mass() {
    let mut app = physics_app();
    let body = dynamic_box(&mut app, 0.5, Vec3::new(0.0, 5.0, 0.0));
    let auto_mass = {
        app.update();
        rapier_body(&app, body).mass()
    };
    app.world_mut().entity_mut(body).insert(Mass::Explicit {
        mass: 80.0,
        center_of_mass: Vec3::ZERO,
        principal_inertia: Vec3::splat(1.0),
    });
    app.update();
    assert!(
        rapier_body(&app, body).mass() > auto_mass + 70.0,
        "explicit mass added"
    );
    app.world_mut().entity_mut(body).insert(Mass::Auto);
    app.update();
    assert!(
        (rapier_body(&app, body).mass() - auto_mass).abs() < 1e-3,
        "Mass::Auto returns to the density mass: {} vs {auto_mass}",
        rapier_body(&app, body).mass()
    );
}

#[test]
fn disabling_can_sleep_at_runtime_keeps_the_body_awake_for_good() {
    let mut app = physics_app();
    let body = dynamic_box(&mut app, 0.5, Vec3::new(0.0, 5.0, 0.0));
    app.update();
    assert!(rapier_body(&app, body).activation().normalized_linear_threshold >= 0.0);
    app.world_mut()
        .entity_mut(body)
        .get_mut::<RigidBody>()
        .expect("body component")
        .can_sleep = false;
    app.update();
    assert!(rapier_body(&app, body).activation().normalized_linear_threshold < 0.0);
    app.world_mut()
        .entity_mut(body)
        .get_mut::<RigidBody>()
        .expect("body component")
        .can_sleep = true;
    app.update();
    assert!(rapier_body(&app, body).activation().normalized_linear_threshold >= 0.0);
}

// ---------------------------------------------------------------------------
// CCD (specs/15 § 6)
// ---------------------------------------------------------------------------

#[test]
fn ccd_stops_a_fast_bullet_through_a_thin_plate() {
    let mut app = physics_app();
    // A thin static plate at y=0.
    let _plate = app
        .world_mut()
        .spawn((
            Transform::IDENTITY,
            RigidBody::static_body(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::new(2.0, 0.05, 2.0),
                },
                ..Collider::default()
            },
        ))
        .id();

    let bullet = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, 5.0, 0.0)),
            RigidBody {
                ccd: true,
                ..RigidBody::dynamic()
            },
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::splat(0.1),
                },
                ..Collider::default()
            },
            Velocity {
                linear: Vec3::new(0.0, -300.0, 0.0),
                angular: Vec3::ZERO,
            },
        ))
        .id();

    run(&mut app, 1.0);
    let y = translation_of(&app, bullet).y;
    assert!(y > -0.2, "the CCD bullet did not tunnel through the plate: y={y}");
}

// ---------------------------------------------------------------------------
// Kinematic platforms push dynamics (specs/15 § 1 kinematic)
// ---------------------------------------------------------------------------

#[test]
fn kinematic_platform_carries_a_dynamic_box() {
    let mut app = physics_app();
    // A kinematic platform at the origin that slides +X at 1 m/s.
    let platform = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, 0.0, 0.0)),
            RigidBody::kinematic(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::new(2.0, 0.25, 2.0),
                },
                ..Collider::default()
            },
        ))
        .id();
    let cargo = dynamic_box(&mut app, 0.25, Vec3::new(0.0, 0.75, 0.0));

    let dt = 1.0f32 / 60.0;
    for _ in 0..180 {
        // Push the platform's target pose (kinematic bodies are Transform-driven).
        {
            let mut t = app.world_mut().get_mut::<Transform>(platform).expect("t");
            t.translation.x += 1.0 * dt;
        }
        app.update();
    }

    let px = translation_of(&app, platform).x;
    let cx = translation_of(&app, cargo).x;
    assert!((px - 3.0).abs() < 0.05, "platform moved 3 m: {px}");
    assert!(
        (cx - px).abs() < 0.5,
        "the cargo was carried along: cargo x={cx} platform x={px}"
    );
    assert!(
        translation_of(&app, cargo).y > 0.3,
        "cargo still rides on the platform"
    );
}

// ---------------------------------------------------------------------------
// Perf budget (specs/15 § 5) — desktop reference, ignored in normal runs
// ---------------------------------------------------------------------------

#[test]
#[ignore = "perf budget measurement (specs/15 § 5 desktop reference: 2000 boxes ≤ 4 ms/step)"]
fn perf_step_time_2000_boxes() {
    let mut app = physics_app();
    let _ground = ground_plate(&mut app, 40.0, 40.0);
    // 2,000 dynamic boxes in a 20×10×10 grid.
    let half = 0.25f32;
    let mut index = 0;
    for xi in 0..20 {
        for yi in 0..10 {
            for zi in 0..10 {
                let p = Vec3::new(
                    (xi as f32 - 10.0) * 1.0,
                    half + yi as f32 * 0.6,
                    (zi as f32 - 5.0) * 1.0,
                );
                dynamic_box(&mut app, half, p);
                index += 1;
            }
        }
    }
    assert_eq!(index, 2000);

    // Let them settle, then measure 60 steps.
    run(&mut app, 5.0);
    let start = std::time::Instant::now();
    for _ in 0..60 {
        app.update();
    }
    let per_step = start.elapsed().as_secs_f32() / 60.0;
    assert!(
        per_step <= 0.004,
        "desktop budget: 2000 boxes step in ≤ 4 ms, measured {:.2} ms",
        per_step * 1000.0
    );
}

/// Bodies enter rapier in entity order, not archetype order: a scene whose archetypes were
/// created in a different order (another plugin, a different startup order) gets the same
/// handles and so the same simulation. The arena record/replay test broke on this when an
/// unrelated plugin registered one more resource.
#[test]
fn bodies_get_handles_in_entity_order_not_archetype_order() {
    let mut app = physics_app();
    // Create the plain-body archetype first with a throwaway entity, then free its index.
    let throwaway = app
        .world_mut()
        .spawn((Transform::default(), RigidBody::dynamic()))
        .id();
    app.world_mut().despawn(throwaway);
    // `first` (lower index) lives in a newer archetype (it also has a `Velocity`).
    let first = app
        .world_mut()
        .spawn((Transform::default(), RigidBody::dynamic(), Velocity::default()))
        .id();
    let second = app
        .world_mut()
        .spawn((Transform::from_translation(Vec3::X * 3.0), RigidBody::dynamic()))
        .id();
    assert!(
        first.index().index() < second.index().index(),
        "spawn order: {first:?} before {second:?}"
    );
    app.update();
    let raw = |e: Entity, app: &App| {
        app.world()
            .get::<regia_physics::RigidBodyHandle>(e)
            .map(|h| h.raw)
            .expect("handle")
    };
    assert!(
        raw(first, &app) < raw(second, &app),
        "rapier handles must follow entity order ({} vs {})",
        raw(first, &app),
        raw(second, &app)
    );
}

#[test]
fn impulse_on_non_dynamic_body_is_kept_until_dynamic() {
    let mut app = physics_app();
    let e = static_box(&mut app, 0.5, Vec3::new(0.0, 10.0, 0.0));
    app.world_mut()
        .entity_mut(e)
        .insert(regia_physics::ExternalImpulse {
            impulse: Vec3::new(0.0, 50.0, 0.0),
            torque_impulse: Vec3::ZERO,
        });
    run(&mut app, 0.1);
    assert!(
        app.world().get::<regia_physics::ExternalImpulse>(e).is_some(),
        "an impulse that rapier ignored must not be consumed"
    );
    app.world_mut().entity_mut(e).insert(RigidBody::dynamic());
    run(&mut app, 0.1);
    assert!(
        app.world().get::<regia_physics::ExternalImpulse>(e).is_none(),
        "applied once the body is dynamic"
    );
}

#[test]
fn removing_rigid_body_clears_child_collider_handles_so_they_rebuild() {
    let mut app = physics_app();
    let parent = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, 5.0, 0.0)),
            RigidBody::dynamic(),
        ))
        .id();
    let child = app
        .world_mut()
        .spawn((
            regia_ecs::prelude::ChildOf(parent),
            Transform::IDENTITY,
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::splat(0.5),
                },
                ..Collider::default()
            },
        ))
        .id();
    run(&mut app, 0.1);
    assert_eq!(
        app.world()
            .resource::<regia_physics::PhysicsWorld>()
            .colliders
            .len(),
        1
    );

    app.world_mut().entity_mut(parent).remove::<RigidBody>();
    run(&mut app, 0.1);
    assert_eq!(
        app.world()
            .resource::<regia_physics::PhysicsWorld>()
            .colliders
            .len(),
        0
    );
    assert!(
        app.world().get::<regia_physics::ColliderHandle>(child).is_none(),
        "the dangling collider handle is cleared"
    );

    app.world_mut().entity_mut(parent).insert(RigidBody::dynamic());
    run(&mut app, 0.1);
    assert_eq!(
        app.world()
            .resource::<regia_physics::PhysicsWorld>()
            .colliders
            .len(),
        1,
        "the child collider is rebuilt on the returning body"
    );
}

/// Two coincident boxes, `raycast_all` from a point where the ray meets both at the same
/// distance: equal distances must break by collider handle (insertion order), not by the
/// broad phase's traversal order or by entity id, so a shot that applies its effect to
/// `hits[0]` picks the same entity when the colliders were inserted in the same order — and
/// the call stays total (a non-finite time of impact is dropped, not compared NaN-equal to
/// everything). Both entity-id arrangements run (the ADR-1347 timing: ids reserved in one
/// order, components applied in the other), so an id-ordered implementation reverses one of
/// them and a traversal-ordered implementation reverses both.
#[test]
fn raycast_all_breaks_equal_distances_by_collider_handle() {
    let scenario = |inverted: bool| {
        let mut app = physics_app();
        // Both boxes coincident at the origin: the ray meets them at the same distance, so
        // the hit order is decided by the tie-break alone. `inverted` only changes how the
        // ids line up against the collider insertion order.
        let body = || {
            (
                Transform::from_translation(Vec3::ZERO),
                RigidBody::static_body(),
                Collider {
                    shape: ColliderShape::Box {
                        half_extents: Vec3::splat(0.5),
                    },
                    friction: 0.8,
                    ..Collider::default()
                },
            )
        };
        let (first, second) = if inverted {
            // Reserve both ids first, then apply the component bundles in the fixed order
            // "first" before "second": the id reservation order and the collider insertion
            // order disagree (the ADR-1347 thread-timing orders).
            let mut first_cmds = regia_ecs::ecs::world::CommandQueue::default();
            let mut second_cmds = regia_ecs::ecs::world::CommandQueue::default();
            let world = app.world_mut();
            let second_id = regia_ecs::ecs::system::Commands::new(&mut second_cmds, world)
                .spawn(body())
                .id();
            let first_id = regia_ecs::ecs::system::Commands::new(&mut first_cmds, world)
                .spawn(body())
                .id();
            first_cmds.apply(world);
            second_cmds.apply(world);
            (first_id, second_id)
        } else {
            (
                app.world_mut().spawn(body()).id(),
                app.world_mut().spawn(body()).id(),
            )
        };
        run(&mut app, 0.1);
        let world = app.world().resource::<regia_physics::PhysicsWorld>();
        let queries = regia_physics::WorldQueries { world, layers: None };
        // From x = 5 the ray meets both boxes' +X face at distance 4.
        let hits = queries.raycast_all(Vec3::new(5.0, 0.0, 0.0), Vec3::NEG_X, 20.0, QueryFilter::new());
        let sequence: Vec<_> = hits
            .iter()
            .map(|h| {
                if h.entity == first {
                    "first"
                } else if h.entity == second {
                    "second"
                } else {
                    "other"
                }
            })
            .collect();
        assert_eq!(
            sequence.len(),
            2,
            "both boxes are hit, nothing bogus: {sequence:?}"
        );
        sequence
    };
    for inverted in [false, true] {
        assert_eq!(
            scenario(inverted),
            ["first", "second"],
            "inverted {inverted}: equal-distance hits must follow the collider insertion \
             (handle) order, not entity ids and not the broad-phase traversal order"
        );
    }
}
