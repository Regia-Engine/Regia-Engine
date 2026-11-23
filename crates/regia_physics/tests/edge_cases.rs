// SPDX-License-Identifier: MPL-2.0
//! Degenerate inputs to the physics API: zero-size and malformed shapes, NaN and zero query rays,
//! layer configs that do not parse, an empty debris budget. A bad input must be a miss or an
//! error, never a rapier panic or a bogus hit.
//!
//! The analytic expectations (a ray straight down onto a box top, a sphere resting in a plate)
//! are elementary geometry derived here.

use std::time::Duration;

use regia_app::{App, FixedTime, TimeUpdateStrategy};
use regia_core::math::{Quat, Vec3};
use regia_ecs::prelude::Entity;
use regia_physics::{
    BodyBudget, Collider, ColliderShape, CollisionLayer, LayerConfigError, LayerFile, MAX_LAYERS,
    PhysicsLayers, PhysicsPlugin, PhysicsWorld, QueryFilter, RigidBody, ShapeAxis, Teleport, Velocity,
    WorldQueries, shape_axis_rotation, shape_to_shared_shape,
};
use regia_scene::Transform;

fn sphere(radius: f32) -> ColliderShape {
    ColliderShape::Sphere { radius }
}

fn boxed(x: f32, y: f32, z: f32) -> ColliderShape {
    ColliderShape::Box {
        half_extents: Vec3::new(x, y, z),
    }
}

#[test]
fn valid_and_zero_size_primitives_build() {
    assert!(shape_to_shared_shape(&boxed(1.0, 1.0, 1.0)).is_some());
    assert!(shape_to_shared_shape(&sphere(0.5)).is_some());
    // Zero is a legal (if useless) size.
    assert!(shape_to_shared_shape(&boxed(0.0, 0.0, 0.0)).is_some());
    assert!(shape_to_shared_shape(&sphere(0.0)).is_some());
    for kind in 0..3 {
        let make = |half_height: f32, radius: f32, axis: ShapeAxis| match kind {
            0 => ColliderShape::Capsule {
                half_height,
                radius,
                axis,
            },
            1 => ColliderShape::Cylinder {
                half_height,
                radius,
                axis,
            },
            _ => ColliderShape::Cone {
                half_height,
                radius,
                axis,
            },
        };
        assert!(shape_to_shared_shape(&make(1.0, 0.5, ShapeAxis::Y)).is_some());
        assert!(shape_to_shared_shape(&make(0.0, 0.5, ShapeAxis::X)).is_some());
        assert!(shape_to_shared_shape(&make(f32::NAN, 0.5, ShapeAxis::Y)).is_none());
        assert!(shape_to_shared_shape(&make(1.0, f32::INFINITY, ShapeAxis::Y)).is_none());
        assert!(shape_to_shared_shape(&make(1.0, -0.5, ShapeAxis::Z)).is_none());
    }
}

#[test]
fn non_finite_and_negative_sizes_are_rejected() {
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -1.0] {
        assert!(shape_to_shared_shape(&sphere(bad)).is_none(), "sphere {bad}");
        assert!(
            shape_to_shared_shape(&boxed(bad, 1.0, 1.0)).is_none(),
            "box x {bad}"
        );
        assert!(
            shape_to_shared_shape(&boxed(1.0, bad, 1.0)).is_none(),
            "box y {bad}"
        );
        assert!(
            shape_to_shared_shape(&boxed(1.0, 1.0, bad)).is_none(),
            "box z {bad}"
        );
    }
}

#[test]
fn convex_hulls_need_finite_points() {
    let tetra = vec![
        Vec3::new(0.0, 0.0, 0.0),
        Vec3::new(1.0, 0.0, 0.0),
        Vec3::new(0.0, 1.0, 0.0),
        Vec3::new(0.0, 0.0, 1.0),
    ];
    assert!(
        shape_to_shared_shape(&ColliderShape::ConvexHull {
            points: tetra.clone()
        })
        .is_some()
    );
    assert!(shape_to_shared_shape(&ColliderShape::ConvexHull { points: Vec::new() }).is_none());
    assert!(
        shape_to_shared_shape(&ColliderShape::ConvexHull {
            points: tetra[..2].to_vec()
        })
        .is_none()
    );
    let mut nan = tetra.clone();
    nan[2] = Vec3::NAN;
    assert!(shape_to_shared_shape(&ColliderShape::ConvexHull { points: nan }).is_none());
    // A flat or repeated point set must not panic (whether it builds is up to the hull code).
    let _ = shape_to_shared_shape(&ColliderShape::ConvexHull {
        points: vec![Vec3::ONE; 5],
    });
    let _ = shape_to_shared_shape(&ColliderShape::ConvexHull {
        points: vec![Vec3::ZERO, Vec3::X, Vec3::new(2.0, 0.0, 0.0)],
    });
}

#[test]
fn trimeshes_validate_their_indices() {
    let verts = vec![Vec3::ZERO, Vec3::X, Vec3::Z];
    let tri = |vertices: Vec<Vec3>, indices: Vec<[u32; 3]>| ColliderShape::TriMesh { vertices, indices };
    assert!(shape_to_shared_shape(&tri(verts.clone(), vec![[0, 1, 2]])).is_some());
    assert!(shape_to_shared_shape(&tri(verts.clone(), Vec::new())).is_none());
    assert!(shape_to_shared_shape(&tri(Vec::new(), vec![[0, 1, 2]])).is_none());
    assert!(shape_to_shared_shape(&tri(verts.clone(), vec![[0, 1, 3]])).is_none());
    assert!(shape_to_shared_shape(&tri(verts.clone(), vec![[0, 1, u32::MAX]])).is_none());
    let mut nan = verts;
    nan[1] = Vec3::NAN;
    assert!(shape_to_shared_shape(&tri(nan, vec![[0, 1, 2]])).is_none());
}

#[test]
fn trimesh_from_triangles_edge_cases() {
    let v = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]];
    assert!(ColliderShape::trimesh_from_triangles(&v, &[0, 1, 2]).is_some());
    assert!(ColliderShape::trimesh_from_triangles(&v, &[]).is_none());
    assert!(ColliderShape::trimesh_from_triangles(&[], &[0, 1, 2]).is_none());
    assert!(ColliderShape::trimesh_from_triangles(&v, &[0, 1]).is_none());
    assert!(ColliderShape::trimesh_from_triangles(&v, &[0, 1, 2, 0]).is_none());
    assert!(ColliderShape::trimesh_from_triangles(&v, &[0, 1, 3]).is_none());
    let two = ColliderShape::trimesh_from_triangles(&v, &[0, 1, 2, 2, 1, 0]).expect("two triangles");
    let ColliderShape::TriMesh { indices, .. } = two else {
        panic!("expected a trimesh");
    };
    assert_eq!(indices, vec![[0, 1, 2], [2, 1, 0]]);
}

#[test]
fn heightfields_validate_their_grid() {
    let hf = |nx: usize, nz: usize, heights: Vec<f32>| ColliderShape::HeightField {
        nx,
        nz,
        heights,
        scale: Vec3::new(10.0, 1.0, 10.0),
    };
    assert!(shape_to_shared_shape(&hf(3, 3, vec![0.0; 9])).is_some());
    assert!(shape_to_shared_shape(&hf(3, 3, vec![0.0; 8])).is_none());
    assert!(shape_to_shared_shape(&hf(3, 3, vec![0.0; 10])).is_none());
    assert!(shape_to_shared_shape(&hf(0, 0, Vec::new())).is_none());
    assert!(shape_to_shared_shape(&hf(1, 5, vec![0.0; 5])).is_none());
    assert!(shape_to_shared_shape(&hf(5, 1, vec![0.0; 5])).is_none());
    // A grid whose cell count overflows usize is a mismatch, not an arithmetic panic.
    assert!(shape_to_shared_shape(&hf(usize::MAX, 2, vec![0.0; 4])).is_none());
    // Infinity is not a hole (NaN is); a non-finite scale is rejected.
    let mut inf = vec![0.0; 9];
    inf[4] = f32::INFINITY;
    assert!(shape_to_shared_shape(&hf(3, 3, inf)).is_none());
    let bad_scale = ColliderShape::HeightField {
        nx: 3,
        nz: 3,
        heights: vec![0.0; 9],
        scale: Vec3::new(f32::NAN, 1.0, 1.0),
    };
    assert!(shape_to_shared_shape(&bad_scale).is_none());
}

#[test]
fn compound_shapes_reject_bad_children_and_offsets() {
    let at = |child: ColliderShape, translation: Vec3| (Transform::from_translation(translation), child);
    assert!(shape_to_shared_shape(&ColliderShape::Compound { shapes: Vec::new() }).is_none());
    let good = ColliderShape::Compound {
        shapes: vec![at(sphere(0.5), Vec3::ZERO), at(boxed(1.0, 1.0, 1.0), Vec3::X)],
    };
    assert!(shape_to_shared_shape(&good).is_some());
    let bad_child = ColliderShape::Compound {
        shapes: vec![at(sphere(0.5), Vec3::ZERO), at(sphere(f32::NAN), Vec3::X)],
    };
    assert!(shape_to_shared_shape(&bad_child).is_none());
    let bad_offset = ColliderShape::Compound {
        shapes: vec![at(sphere(0.5), Vec3::NAN)],
    };
    assert!(shape_to_shared_shape(&bad_offset).is_none());
    let mut rotated = Transform::IDENTITY;
    rotated.rotation = Quat::from_xyzw(f32::NAN, 0.0, 0.0, 1.0);
    let bad_rotation = ColliderShape::Compound {
        shapes: vec![(rotated, sphere(0.5))],
    };
    assert!(shape_to_shared_shape(&bad_rotation).is_none());
    // Nested compounds recurse, including into the validity check.
    let nested = ColliderShape::Compound {
        shapes: vec![at(bad_child, Vec3::ZERO)],
    };
    assert!(shape_to_shared_shape(&nested).is_none());
}

#[test]
fn axis_rotation_is_identity_for_axisless_shapes() {
    assert_eq!(shape_axis_rotation(&sphere(1.0)), Quat::IDENTITY);
    assert_eq!(shape_axis_rotation(&boxed(1.0, 1.0, 1.0)), Quat::IDENTITY);
}

// ---------------------------------------------------------------------------
// Scene queries
// ---------------------------------------------------------------------------

/// A physics app with one static plate (top face at y = 0) synced into the rapier world.
fn app_with_plate() -> (App, Entity) {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    let mut fixed = FixedTime::default();
    fixed.hz = 60.0;
    app.insert_resource(fixed).add_plugins(PhysicsPlugin);
    let plate = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
            RigidBody::static_body(),
            Collider {
                shape: boxed(5.0, 0.5, 5.0),
                ..Collider::default()
            },
        ))
        .id();
    for _ in 0..6 {
        app.update();
    }
    (app, plate)
}

fn queries(app: &App) -> WorldQueries<'_> {
    WorldQueries {
        world: app.world().resource::<PhysicsWorld>(),
        layers: None,
    }
}

const UP: Vec3 = Vec3::new(0.0, 10.0, 0.0);

#[test]
fn a_straight_ray_hits_the_plate_top() {
    let (app, plate) = app_with_plate();
    let q = queries(&app);
    let hit = q.raycast(UP, Vec3::NEG_Y, 20.0, QueryFilter::new()).expect("hit");
    assert_eq!(hit.entity, plate);
    assert!((hit.distance - 10.0).abs() < 1e-3);
    assert!((hit.normal - Vec3::Y).length() < 1e-3);
    // The direction is normalized by the call.
    let long = q
        .raycast(UP, Vec3::new(0.0, -7.0, 0.0), 20.0, QueryFilter::new())
        .expect("hit");
    assert!((long.distance - 10.0).abs() < 1e-3);
    // Out of range, pointing away, and past the plate's edge all miss.
    assert!(q.raycast(UP, Vec3::NEG_Y, 5.0, QueryFilter::new()).is_none());
    assert!(q.raycast(UP, Vec3::Y, 20.0, QueryFilter::new()).is_none());
    assert!(
        q.raycast(Vec3::new(9.0, 10.0, 0.0), Vec3::NEG_Y, 20.0, QueryFilter::new())
            .is_none()
    );
}

#[test]
fn rays_with_bad_numbers_miss() {
    let (app, _) = app_with_plate();
    let q = queries(&app);
    let miss = |origin: Vec3, dir: Vec3, max: f32| {
        q.raycast(origin, dir, max, QueryFilter::new()).is_none()
            && q.raycast_all(origin, dir, max, QueryFilter::new()).is_empty()
    };
    assert!(miss(UP, Vec3::ZERO, 20.0), "zero direction");
    assert!(miss(UP, Vec3::NAN, 20.0), "NaN direction");
    assert!(miss(Vec3::NAN, Vec3::NEG_Y, 20.0), "NaN origin");
    assert!(
        miss(Vec3::new(0.0, f32::INFINITY, 0.0), Vec3::NEG_Y, 20.0),
        "infinite origin"
    );
    assert!(miss(UP, Vec3::NEG_Y, f32::NAN), "NaN distance");
    assert!(miss(UP, Vec3::NEG_Y, -1.0), "negative distance");
    assert!(miss(UP, Vec3::NEG_Y, 0.0), "zero distance from 10 m away");
}

#[test]
fn raycast_all_is_sorted_and_includes_every_collider_crossed() {
    let (mut app, plate) = app_with_plate();
    let upper = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, 4.5, 0.0)),
            RigidBody::static_body(),
            Collider {
                shape: boxed(1.0, 0.5, 1.0),
                ..Collider::default()
            },
        ))
        .id();
    for _ in 0..6 {
        app.update();
    }
    let hits = queries(&app).raycast_all(UP, Vec3::NEG_Y, 20.0, QueryFilter::new());
    let entities: Vec<Entity> = hits.iter().map(|h| h.entity).collect();
    assert!(
        entities.contains(&plate) && entities.contains(&upper),
        "{entities:?}"
    );
    assert!(hits.windows(2).all(|w| w[0].distance <= w[1].distance));
    // Excluding the upper box leaves the plate as the nearest hit.
    let excluded = [upper];
    let nearest = queries(&app)
        .raycast(
            UP,
            Vec3::NEG_Y,
            20.0,
            QueryFilter::new().exclude_entities(&excluded),
        )
        .expect("plate");
    assert_eq!(nearest.entity, plate);
    // A predicate that refuses everything leaves nothing to hit.
    let nobody = |_: Entity| false;
    assert!(
        queries(&app)
            .raycast(UP, Vec3::NEG_Y, 20.0, QueryFilter::new().predicate(&nobody))
            .is_none()
    );
}

#[test]
fn overlap_lists_entities_in_entity_order() {
    // The broad phase's order follows rapier handle history; gameplay applying one-shot effects
    // in the returned order needs the same order every run (ADR-1660).
    let (mut app, plate) = app_with_plate();
    let mut boxes = Vec::new();
    for i in 0..6 {
        // Spawned right to left, so handle and position orders disagree.
        let x = 2.0 - i as f32 * 0.8;
        let e = app
            .world_mut()
            .spawn((
                Transform::from_translation(Vec3::new(x, 0.3, 0.0)),
                RigidBody::static_body(),
                Collider {
                    shape: boxed(0.3, 0.3, 0.3),
                    ..Collider::default()
                },
            ))
            .id();
        boxes.push(e);
    }
    // Despawn and respawn one so a rapier slot is reused out of order.
    app.world_mut().despawn(boxes[1]);
    app.update();
    boxes[1] = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(1.2, 0.3, 0.0)),
            RigidBody::static_body(),
            Collider {
                shape: boxed(0.3, 0.3, 0.3),
                ..Collider::default()
            },
        ))
        .id();
    for _ in 0..3 {
        app.update();
    }
    let all = queries(&app).overlap(&boxed(4.0, 1.0, 1.0), Transform::default(), QueryFilter::new());
    let mut expected = boxes.clone();
    expected.push(plate);
    expected.sort_by_key(|e| (e.index().index(), e.to_bits()));
    assert_eq!(all, expected);
}

#[test]
fn shape_queries_with_bad_numbers_are_empty() {
    let (app, plate) = app_with_plate();
    let q = queries(&app);
    let at = |y: f32| Transform::from_translation(Vec3::new(0.0, y, 0.0));
    let ball = sphere(0.5);

    // Sanity: a ball resting in the plate overlaps it, one high above does not.
    assert_eq!(q.overlap(&ball, at(0.0), QueryFilter::new()), vec![plate]);
    assert!(q.overlap(&ball, at(5.0), QueryFilter::new()).is_empty());
    let cast = q
        .shapecast(&ball, at(5.0), Vec3::NEG_Y, 10.0, QueryFilter::new())
        .expect("hit");
    assert_eq!(cast.entity, plate);
    assert!(
        (cast.time_of_impact - 4.5).abs() < 1e-2,
        "{}",
        cast.time_of_impact
    );

    // Invalid shapes and poses.
    let bad_shape = sphere(f32::NAN);
    assert!(q.overlap(&bad_shape, at(0.0), QueryFilter::new()).is_empty());
    assert!(
        q.shapecast(&bad_shape, at(5.0), Vec3::NEG_Y, 10.0, QueryFilter::new())
            .is_none()
    );
    let nan_pose = Transform::from_translation(Vec3::NAN);
    assert!(q.overlap(&ball, nan_pose, QueryFilter::new()).is_empty());
    assert!(
        q.shapecast(&ball, nan_pose, Vec3::NEG_Y, 10.0, QueryFilter::new())
            .is_none()
    );
    assert!(
        q.shapecast(&ball, at(5.0), Vec3::NAN, 10.0, QueryFilter::new())
            .is_none()
    );
    assert!(
        q.shapecast(&ball, at(5.0), Vec3::NEG_Y, f32::NAN, QueryFilter::new())
            .is_none()
    );
    assert!(
        q.shapecast(&ball, at(5.0), Vec3::NEG_Y, -1.0, QueryFilter::new())
            .is_none()
    );
}

#[test]
fn closest_point_edge_cases() {
    let (app, plate) = app_with_plate();
    let q = queries(&app);
    let above = q
        .closest_point(Vec3::new(0.0, 3.0, 0.0), 10.0, QueryFilter::new())
        .expect("found");
    assert_eq!(above.entity, plate);
    assert!((above.point - Vec3::ZERO).length() < 1e-3);
    assert!(!above.is_inside);
    // Beyond the search radius, and non-finite arguments, find nothing.
    assert!(
        q.closest_point(Vec3::new(0.0, 30.0, 0.0), 10.0, QueryFilter::new())
            .is_none()
    );
    assert!(q.closest_point(Vec3::NAN, 10.0, QueryFilter::new()).is_none());
    assert!(
        q.closest_point(Vec3::ZERO, f32::NAN, QueryFilter::new())
            .is_none()
    );
    assert!(
        q.closest_point(Vec3::new(0.0, 3.0, 0.0), -1.0, QueryFilter::new())
            .is_none()
    );
}

#[test]
fn queries_on_an_empty_world_miss() {
    let world = PhysicsWorld::default();
    let q = WorldQueries {
        world: &world,
        layers: None,
    };
    assert!(q.raycast(UP, Vec3::NEG_Y, 100.0, QueryFilter::new()).is_none());
    assert!(
        q.raycast_all(UP, Vec3::NEG_Y, 100.0, QueryFilter::new())
            .is_empty()
    );
    assert!(q.closest_point(UP, 100.0, QueryFilter::new()).is_none());
    assert!(
        q.overlap(&sphere(1.0), Transform::IDENTITY, QueryFilter::new())
            .is_empty()
    );
    assert!(
        q.shapecast(
            &sphere(1.0),
            Transform::IDENTITY,
            Vec3::X,
            5.0,
            QueryFilter::new()
        )
        .is_none()
    );
}

#[test]
fn invalid_and_zero_size_colliders_do_not_break_the_step() {
    let (mut app, plate) = app_with_plate();
    let spawn = |app: &mut App, shape: ColliderShape, pos: Vec3| {
        app.world_mut()
            .spawn((
                Transform::from_translation(pos),
                RigidBody::static_body(),
                Collider {
                    shape,
                    ..Collider::default()
                },
            ))
            .id()
    };
    let zero_box = spawn(&mut app, boxed(0.0, 0.0, 0.0), Vec3::new(2.0, 1.0, 0.0));
    let zero_ball = spawn(&mut app, sphere(0.0), Vec3::new(-2.0, 1.0, 0.0));
    let nan_ball = spawn(&mut app, sphere(f32::NAN), Vec3::new(0.0, 1.0, 2.0));
    let empty_hull = spawn(
        &mut app,
        ColliderShape::ConvexHull { points: Vec::new() },
        Vec3::new(0.0, 1.0, -2.0),
    );
    let bad_mesh = spawn(
        &mut app,
        ColliderShape::TriMesh {
            vertices: vec![Vec3::ZERO, Vec3::X, Vec3::Z],
            indices: vec![[0, 1, 9]],
        },
        Vec3::new(3.0, 1.0, 3.0),
    );
    for _ in 0..30 {
        app.update();
    }
    // The world is intact: the plate still answers queries, and every entity still exists.
    let hit = queries(&app)
        .raycast(UP, Vec3::NEG_Y, 20.0, QueryFilter::new())
        .expect("plate");
    assert_eq!(hit.entity, plate);
    for e in [zero_box, zero_ball, nan_ball, empty_hull, bad_mesh] {
        assert!(app.world().get::<Collider>(e).is_some());
    }
}

// ---------------------------------------------------------------------------
// Non-finite input to the physics step (ADR-1507, the half of ADR-383 it leaves open)
// ---------------------------------------------------------------------------

/// A dynamic ball with a `Velocity` component, at `pos`.
fn dynamic_ball(app: &mut App, pos: Vec3) -> Entity {
    app.world_mut()
        .spawn((
            Transform::from_translation(pos),
            RigidBody::dynamic(),
            Velocity::default(),
            Collider {
                shape: sphere(0.5),
                ..Collider::default()
            },
        ))
        .id()
}

/// Every rapier pose and velocity in the world is finite.
fn all_poses_finite(physics: &PhysicsWorld) -> bool {
    physics.bodies.iter().all(|(_, rb)| {
        let pose = rb.position();
        let t = pose.translation;
        let r = pose.rotation;
        let lin = rb.linvel();
        let ang = rb.angvel();
        t.x.is_finite()
            && t.y.is_finite()
            && t.z.is_finite()
            && r.x.is_finite()
            && r.y.is_finite()
            && r.z.is_finite()
            && r.w.is_finite()
            && lin.x.is_finite()
            && lin.y.is_finite()
            && lin.z.is_finite()
            && ang.x.is_finite()
            && ang.y.is_finite()
            && ang.z.is_finite()
    })
}

#[test]
fn a_non_finite_spawn_transform_parks_the_body_at_the_origin() {
    let (mut app, _plate) = app_with_plate();
    let body = dynamic_ball(&mut app, Vec3::NAN);
    for _ in 0..10 {
        app.update();
    }
    let physics = app.world().resource::<PhysicsWorld>();
    assert!(
        physics.stats().non_finite_inputs >= 1,
        "the NaN spawn pose was counted"
    );
    assert!(all_poses_finite(physics), "no NaN reached rapier");
    let transform = app.world().get::<Transform>(body).expect("transform");
    assert!(
        transform.translation.is_finite() && transform.rotation.is_finite(),
        "the write-back replaces the NaN scene transform with the simulated pose"
    );
}

#[test]
fn a_non_finite_velocity_write_is_ignored_and_counted() {
    let (mut app, _plate) = app_with_plate();
    let body = dynamic_ball(&mut app, Vec3::new(0.0, 3.0, 0.0));
    for _ in 0..6 {
        app.update();
    }
    let before = app.world().resource::<PhysicsWorld>().stats().non_finite_inputs;
    app.world_mut()
        .get_mut::<Velocity>(body)
        .expect("velocity")
        .linear = Vec3::new(0.0, f32::NAN, 0.0);
    for _ in 0..6 {
        app.update();
    }
    let physics = app.world().resource::<PhysicsWorld>();
    assert!(
        physics.stats().non_finite_inputs > before,
        "the NaN velocity was counted"
    );
    assert!(
        all_poses_finite(physics),
        "the body keeps its last valid velocity"
    );
    let velocity = app.world().get::<Velocity>(body).expect("velocity");
    assert!(
        velocity.linear.is_finite() && velocity.angular.is_finite(),
        "the read-back never writes NaN into the component"
    );
}

#[test]
fn a_non_finite_kinematic_transform_keeps_the_last_pose() {
    let (mut app, _plate) = app_with_plate();
    let body = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(2.0, 2.0, 0.0)),
            RigidBody::kinematic(),
            Collider {
                shape: sphere(0.5),
                ..Collider::default()
            },
        ))
        .id();
    for _ in 0..4 {
        app.update();
    }
    app.world_mut()
        .get_mut::<Transform>(body)
        .expect("transform")
        .translation = Vec3::NAN;
    for _ in 0..4 {
        app.update();
    }
    let physics = app.world().resource::<PhysicsWorld>();
    assert!(
        physics.stats().non_finite_inputs >= 1,
        "the skipped kinematic push was counted"
    );
    let mut kept_its_pose = false;
    for (_, rb) in physics.bodies.iter() {
        let t = rb.position().translation;
        assert!(t.x.is_finite() && t.y.is_finite() && t.z.is_finite());
        if rb.is_kinematic() && (t.x - 2.0).abs() < 1e-4 && (t.y - 2.0).abs() < 1e-4 {
            kept_its_pose = true;
        }
    }
    assert!(kept_its_pose, "the body stays at its last valid pose");
}

#[test]
fn a_non_finite_teleport_is_ignored_and_the_marker_is_consumed() {
    let (mut app, _plate) = app_with_plate();
    let body = dynamic_ball(&mut app, Vec3::new(0.0, 3.0, 0.0));
    for _ in 0..90 {
        app.update();
    }
    // Settled on the plate (top at y = 0, ball radius 0.5): 0.4 .. 0.6.
    let settled_y = app
        .world()
        .get::<Transform>(body)
        .expect("transform")
        .translation
        .y;
    assert!(
        (settled_y - 0.5).abs() < 0.1,
        "the ball settles on the plate first (y = {settled_y})"
    );
    app.world_mut()
        .get_mut::<Transform>(body)
        .expect("transform")
        .translation = Vec3::new(f32::INFINITY, f32::INFINITY, 0.0);
    app.world_mut().entity_mut(body).insert(Teleport);
    for _ in 0..6 {
        app.update();
    }
    assert!(
        app.world().get::<Teleport>(body).is_none(),
        "the marker is consumed even though the teleport was refused"
    );
    let physics = app.world().resource::<PhysicsWorld>();
    assert!(
        physics.stats().non_finite_inputs >= 1,
        "the refused teleport was counted"
    );
    assert!(all_poses_finite(physics));
    let y = app
        .world()
        .get::<Transform>(body)
        .expect("transform")
        .translation
        .y;
    assert!(
        (y - settled_y).abs() < 0.1,
        "the body stays where it was (y = {y})"
    );
}

// ---------------------------------------------------------------------------
// Layers
// ---------------------------------------------------------------------------

fn layer_file(layers: &[&str], collides: &[(&str, &str)], queries: &[(&str, &str)]) -> LayerFile {
    let own = |s: &str| s.to_string();
    LayerFile {
        layers: layers.iter().map(|s| (*s).to_string()).collect(),
        collides: collides.iter().map(|(a, b)| (own(a), own(b))).collect(),
        queries: queries.iter().map(|(a, b)| (own(a), own(b))).collect(),
    }
}

#[test]
fn layer_config_rejects_garbage() {
    assert!(matches!(LayerFile::parse(""), Err(LayerConfigError::Parse(_))));
    assert!(matches!(
        LayerFile::parse("not ron"),
        Err(LayerConfigError::Parse(_))
    ));
    assert!(matches!(
        LayerFile::parse(r#"(layers: [], collides: [], queries: [], surprise: 1)"#),
        Err(LayerConfigError::Parse(_))
    ));
    // Missing fields.
    assert!(matches!(
        LayerFile::parse("(layers: [])"),
        Err(LayerConfigError::Parse(_))
    ));
    // An empty but well-formed config is legal and builds an empty table.
    let empty = LayerFile::parse("(layers: [], collides: [], queries: [])").expect("parses");
    let layers = PhysicsLayers::from_file(&empty).expect("builds");
    assert!(layers.names.is_empty());
    assert_eq!(layers.index_of("anything"), None);
    assert!(layers.collision_rows.iter().all(|&row| row == 0));
}

#[test]
fn layer_config_load_reports_a_missing_file() {
    // A path under a directory that is never created: reading it can only fail.
    let missing = std::env::temp_dir()
        .join(format!("regia_physics_never_created_{}", std::process::id()))
        .join("nope.ron");
    assert!(matches!(LayerFile::load(&missing), Err(LayerConfigError::Io(_))));
}

#[test]
fn layer_table_limits_and_unknown_names() {
    let names: Vec<String> = (0..=MAX_LAYERS).map(|i| format!("l{i}")).collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    assert!(matches!(
        PhysicsLayers::from_file(&layer_file(&refs, &[], &[])),
        Err(LayerConfigError::TooManyLayers(n)) if n == MAX_LAYERS + 1
    ));
    // Exactly MAX_LAYERS is fine, including a pair that touches the last bit.
    let full = &refs[..MAX_LAYERS];
    let last = full[MAX_LAYERS - 1];
    let table =
        PhysicsLayers::from_file(&layer_file(full, &[("l0", last)], &[(last, "l0")])).expect("builds");
    assert_eq!(table.collision_rows[0], 1 << (MAX_LAYERS - 1));
    assert_eq!(table.collision_rows[MAX_LAYERS - 1], 1);
    assert_eq!(table.query_rows[MAX_LAYERS - 1], 1);
    assert_eq!(table.query_rows[0], 0, "queries are one-directional");

    assert!(matches!(
        PhysicsLayers::from_file(&layer_file(&["a"], &[("a", "ghost")], &[])),
        Err(LayerConfigError::UnknownLayer(n)) if n == "ghost"
    ));
    assert!(matches!(
        PhysicsLayers::from_file(&layer_file(&["a"], &[], &[("ghost", "a")])),
        Err(LayerConfigError::UnknownLayer(n)) if n == "ghost"
    ));
}

#[test]
fn layer_indices_past_the_table_wrap_instead_of_panicking() {
    let layers = PhysicsLayers::default();
    for index in [0_u16, 1, 31, 32, 33, u16::MAX] {
        let layer = CollisionLayer::new(index);
        let _ = layers.interaction_groups(&layer);
        let _ = layers.query_groups(Some(&layer), Some(u32::MAX));
        let _ = layers.query_groups(Some(&layer), None);
    }
    let _ = layers.query_groups(None, None);
}

#[test]
fn duplicate_layer_names_resolve_to_the_first() {
    let table = PhysicsLayers::from_file(&layer_file(&["a", "b", "a"], &[("a", "b")], &[])).expect("builds");
    assert_eq!(table.index_of("a"), Some(0));
    assert_eq!(table.collision_rows[0], 0b10);
    assert_eq!(table.collision_rows[2], 0, "the shadowed duplicate never matches");
}

// ---------------------------------------------------------------------------
// Debris budget
// ---------------------------------------------------------------------------

#[test]
fn body_budget_edge_cases() {
    let mut budget = BodyBudget::default();
    assert!(budget.is_empty());
    assert!(budget.enforce(0).is_empty());
    assert!(budget.enforce(u32::MAX).is_empty());
    budget.remove(Entity::PLACEHOLDER);

    let mut scratch = regia_ecs::prelude::World::new();
    let ids: Vec<Entity> = (0..4).map(|_| scratch.spawn_empty().id()).collect();
    for &e in &ids {
        budget.push(e);
    }
    assert_eq!(budget.len(), 4);
    // Within budget: nothing is evicted. Over budget: the oldest go first.
    assert!(budget.enforce(4).is_empty());
    assert_eq!(budget.enforce(2), vec![ids[0], ids[1]]);
    assert_eq!(budget.len(), 2);
    // A removed entity no longer counts, and a second removal is harmless.
    budget.remove(ids[2]);
    budget.remove(ids[2]);
    assert_eq!(budget.len(), 1);
    // A zero budget evicts everything that is left.
    assert_eq!(budget.enforce(0), vec![ids[3]]);
    assert!(budget.is_empty());
}
