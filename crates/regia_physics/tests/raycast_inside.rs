// SPDX-License-Identifier: MPL-2.0
//! `raycast` and `raycast_all` from a ray that starts inside a shape (ADR-8721): the first
//! surface the ray crosses, never the ray origin. A ray that starts on a surface keeps its
//! distance-0 hit, and a ray from outside is unchanged. Also `point_velocity`, the ground
//! velocity foot placement reads at a probe hit.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_physics::{
    Collider, ColliderShape, PhysicsPlugin, PhysicsQuery, PhysicsStep, QueryFilter, RaycastHit, RigidBody,
    Velocity,
};

/// One ray: origin, direction, length.
type RayCase = (Vec3, Vec3, f32);

/// What each case reported: the closest hit and every hit.
type Results = Vec<(Option<RaycastHit>, Vec<RaycastHit>)>;

/// A static box with its top face at y = 0 and its bottom at y = -2 (4 m wide), plus a second
/// box under it, top face y = -2.5 and bottom y = -3.5. Runs every ray after the physics step.
fn cast(rays: &[RayCase]) -> Results {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    let mut fixed = regia_app::FixedTime::default();
    fixed.hz = 60.0;
    app.insert_resource(fixed);
    app.add_plugins(PhysicsPlugin);
    for (center, half) in [
        (Vec3::new(0.0, -1.0, 0.0), Vec3::new(2.0, 1.0, 2.0)),
        (Vec3::new(0.0, -3.0, 0.0), Vec3::new(2.0, 0.5, 2.0)),
    ] {
        app.world_mut().spawn((
            regia_scene::Transform::from_translation(center),
            RigidBody::static_body(),
            Collider {
                shape: ColliderShape::Box { half_extents: half },
                ..Collider::default()
            },
        ));
    }
    let slot: Arc<Mutex<Results>> = Arc::new(Mutex::new(Vec::new()));
    let out = slot.clone();
    let count = rays.len();
    let rays = rays.to_vec();
    app.add_systems(
        FixedUpdate,
        (move |queries: PhysicsQuery| {
            let results = rays
                .iter()
                .map(|&(origin, dir, len)| {
                    (
                        queries.raycast(origin, dir, len, QueryFilter::new()),
                        queries.raycast_all(origin, dir, len, QueryFilter::new()),
                    )
                })
                .collect();
            *out.lock().unwrap_or_else(|p| p.into_inner()) = results;
        })
        .after(PhysicsStep),
    );
    for _ in 0..6 {
        app.update();
    }
    let results = slot.lock().unwrap_or_else(|p| p.into_inner()).clone();
    assert_eq!(results.len(), count, "the query system did not run");
    results
}

#[test]
fn a_ray_from_inside_reports_the_face_it_leaves_by() {
    let results = cast(&[
        // Inside the upper box, down: leaves it through its bottom face at y = -2.
        (Vec3::new(0.0, -0.4, 0.0), Vec3::NEG_Y, 3.0),
        // Inside, up: leaves through the top face at y = 0.
        (Vec3::new(0.0, -0.4, 0.0), Vec3::Y, 3.0),
        // Inside, but the ray ends before the bottom face: nothing is crossed.
        (Vec3::new(0.0, -0.4, 0.0), Vec3::NEG_Y, 1.0),
    ]);
    let (closest, all) = &results[0];
    let closest = closest.expect("down from inside crosses the bottom face");
    assert!(
        (closest.distance - 1.6).abs() < 1.0e-3 && (closest.point.y + 2.0).abs() < 1.0e-3,
        "{closest:?}"
    );
    assert_eq!(
        all.len(),
        2,
        "the upper box's exit and the lower box's top: {all:?}"
    );
    assert!((all[0].distance - closest.distance).abs() < 1.0e-5 && all[0].entity == closest.entity);
    assert!((all[1].point.y + 2.5).abs() < 1.0e-3, "{:?}", all[1]);

    let up = results[1].0.expect("up from inside crosses the top face");
    assert!(
        (up.distance - 0.4).abs() < 1.0e-3 && up.point.y.abs() < 1.0e-3,
        "{up:?}"
    );

    let (short, short_all) = &results[2];
    assert!(short.is_none(), "{short:?}");
    assert!(short_all.is_empty(), "{short_all:?}");
}

/// A ray that leaves a shape must report that face's outward normal. Parry's hollow cast
/// (the inside-start path) points the normal back into the shape: a downward ray from inside
/// the upper box hits the bottom face at y = -2 with normal +Y. `GroundProbes` keeps a hit
/// whose normal opposes the ray, so that inward normal is accepted as a floor and the foot
/// plants on the far side of the volume.
#[test]
fn an_exit_face_normal_points_out_of_the_shape() {
    let results = cast(&[
        (Vec3::new(0.0, -0.4, 0.0), Vec3::NEG_Y, 3.0),
        (Vec3::new(0.0, -0.4, 0.0), Vec3::Y, 3.0),
    ]);
    let down = results[0].0.expect("down from inside crosses the bottom face");
    assert!(
        (down.point.y + 2.0).abs() < 1.0e-3,
        "expected the bottom face, got {down:?}"
    );
    assert!(
        down.normal.y < -0.5,
        "bottom exit normal must point out of the box (down), got {down:?}"
    );
    let down_all = &results[0].1[0];
    assert!(
        down_all.normal.y < -0.5,
        "raycast_all exit normal must match, got {down_all:?}"
    );
    let up = results[1].0.expect("up from inside crosses the top face");
    assert!(up.point.y.abs() < 1.0e-3, "{up:?}");
    assert!(
        up.normal.y > 0.5,
        "top exit normal must point out of the box (up), got {up:?}"
    );
}

/// Same inward normal on a sphere (parry negates the outward normal when the ray started
/// inside). Down from the centre leaves through the bottom; the outward normal is -Y.
#[test]
fn a_sphere_exit_normal_points_out() {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    let mut fixed = regia_app::FixedTime::default();
    fixed.hz = 60.0;
    app.insert_resource(fixed);
    app.add_plugins(PhysicsPlugin);
    app.world_mut().spawn((
        regia_scene::Transform::from_translation(Vec3::new(0.0, -1.0, 0.0)),
        RigidBody::static_body(),
        Collider {
            shape: ColliderShape::Sphere { radius: 1.0 },
            ..Collider::default()
        },
    ));
    let slot: Arc<Mutex<Option<RaycastHit>>> = Arc::new(Mutex::new(None));
    let out = slot.clone();
    app.add_systems(
        FixedUpdate,
        (move |queries: PhysicsQuery| {
            *out.lock().unwrap_or_else(|p| p.into_inner()) =
                queries.raycast(Vec3::new(0.0, -0.4, 0.0), Vec3::NEG_Y, 3.0, QueryFilter::new());
        })
        .after(PhysicsStep),
    );
    for _ in 0..6 {
        app.update();
    }
    let hit = (*slot.lock().unwrap_or_else(|p| p.into_inner())).expect("the query system ran");
    assert!(
        (hit.point.y + 2.0).abs() < 1.0e-2,
        "expected the bottom of the sphere, got {hit:?}"
    );
    assert!(
        hit.normal.y < -0.5,
        "sphere exit normal must point out (down), got {hit:?}"
    );
}

#[test]
fn a_ray_on_a_surface_or_outside_is_unchanged() {
    let results = cast(&[
        // On the top face, down: the surface it starts on, at distance 0.
        (Vec3::new(0.5, 0.0, 0.5), Vec3::NEG_Y, 3.0),
        // From above, down: the top face at distance 1.
        (Vec3::new(0.5, 1.0, 0.5), Vec3::NEG_Y, 3.0),
        // In the gap between the boxes, down: the lower box's top face.
        (Vec3::new(0.0, -2.2, 0.0), Vec3::NEG_Y, 3.0),
    ]);
    let on = results[0].0.expect("a ray from the surface touches it");
    assert!(on.distance.abs() < 1.0e-3 && on.point.y.abs() < 1.0e-3, "{on:?}");
    let above = results[1].0.expect("hit from above");
    assert!(
        (above.distance - 1.0).abs() < 1.0e-3 && above.point.y.abs() < 1.0e-3,
        "{above:?}"
    );
    let gap = results[2].0.expect("hit from the gap");
    assert!((gap.distance - 0.3).abs() < 1.0e-3, "{gap:?}");
    assert_eq!(
        results[1].1.len(),
        1,
        "the lower box is past the ray: {:?}",
        results[1].1
    );
}

/// `point_velocity` reads the hit body's motion at a point (the ground velocity foot placement
/// carries a planted foot by): linear velocity plus spin about the center of mass, zero for a
/// static body.
#[test]
fn point_velocity_is_the_body_motion_at_the_point() {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    let mut fixed = regia_app::FixedTime::default();
    fixed.hz = 60.0;
    app.insert_resource(fixed);
    app.add_plugins(PhysicsPlugin);
    let ground = app
        .world_mut()
        .spawn((
            regia_scene::Transform::from_translation(Vec3::new(0.0, -1.0, 0.0)),
            RigidBody::static_body(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::new(2.0, 1.0, 2.0),
                },
                ..Collider::default()
            },
        ))
        .id();
    let spinning = app
        .world_mut()
        .spawn((
            regia_scene::Transform::from_translation(Vec3::new(0.0, 20.0, 0.0)),
            RigidBody::dynamic(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::splat(0.5),
                },
                ..Collider::default()
            },
            Velocity {
                linear: Vec3::new(1.0, 0.0, 0.0),
                angular: Vec3::new(0.0, 2.0, 0.0),
            },
        ))
        .id();
    type Seen = Option<(Vec3, Vec3, Vec3, Vec3)>;
    let slot: Arc<Mutex<Seen>> = Arc::new(Mutex::new(None));
    let out = slot.clone();
    app.add_systems(
        FixedUpdate,
        (move |queries: PhysicsQuery, bodies: Query<(&regia_scene::Transform, &Velocity)>| {
            let Ok((transform, velocity)) = bodies.get(spinning) else {
                return;
            };
            let point = transform.translation + Vec3::new(0.5, 0.0, 0.0);
            let expected = velocity.linear + velocity.angular.cross(point - transform.translation);
            *out.lock().unwrap_or_else(|p| p.into_inner()) = Some((
                queries.point_velocity(spinning, point),
                expected,
                queries.point_velocity(ground, Vec3::ZERO),
                queries.point_velocity(Entity::PLACEHOLDER, Vec3::ZERO),
            ));
        })
        .after(PhysicsStep),
    );
    for _ in 0..4 {
        app.update();
    }
    let (got, expected, still, unknown) = slot
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .expect("the query system ran");
    assert!(
        (got - expected).length() < 1.0e-3 && expected.length() > 1.0,
        "{got:?} vs {expected:?}"
    );
    assert_eq!(still, Vec3::ZERO);
    assert_eq!(unknown, Vec3::ZERO);
}

/// ADR-8801: only the face of a shape the ray started inside is turned along the ray. A nearer
/// surface of another shape is an entry and keeps its outward normal: a small box nested inside
/// the upper box, top face y = -0.8, is what a downward ray from y = -0.4 meets first, with normal
/// +Y (a real floor), and `raycast_all` reports the upper box's exit (-Y) and the lower box's top
/// (+Y) with their outward normals too.
#[test]
fn a_nearer_entry_keeps_its_outward_normal() {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    let mut fixed = regia_app::FixedTime::default();
    fixed.hz = 60.0;
    app.insert_resource(fixed);
    app.add_plugins(PhysicsPlugin);
    for (center, half) in [
        (Vec3::new(0.0, -1.0, 0.0), Vec3::new(2.0, 1.0, 2.0)),
        (Vec3::new(0.0, -3.0, 0.0), Vec3::new(2.0, 0.5, 2.0)),
        (Vec3::new(0.0, -1.0, 0.0), Vec3::new(0.5, 0.2, 0.5)),
    ] {
        app.world_mut().spawn((
            regia_scene::Transform::from_translation(center),
            RigidBody::static_body(),
            Collider {
                shape: ColliderShape::Box { half_extents: half },
                ..Collider::default()
            },
        ));
    }
    type Out = (Option<RaycastHit>, Vec<RaycastHit>);
    let slot: Arc<Mutex<Option<Out>>> = Arc::new(Mutex::new(None));
    let out = slot.clone();
    app.add_systems(
        FixedUpdate,
        (move |queries: PhysicsQuery| {
            let origin = Vec3::new(0.0, -0.4, 0.0);
            *out.lock().unwrap_or_else(|p| p.into_inner()) = Some((
                queries.raycast(origin, Vec3::NEG_Y, 4.0, QueryFilter::new()),
                queries.raycast_all(origin, Vec3::NEG_Y, 4.0, QueryFilter::new()),
            ));
        })
        .after(PhysicsStep),
    );
    for _ in 0..6 {
        app.update();
    }
    let (closest, all) = slot
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone()
        .expect("the query system ran");
    let closest = closest.expect("the nested box's top is under the origin");
    assert!(
        (closest.point.y + 0.8).abs() < 1.0e-3 && closest.normal.y > 0.99,
        "the nested box's top face, normal +Y: {closest:?}"
    );
    let by_y = |y: f32| {
        all.iter()
            .find(|hit| (hit.point.y - y).abs() < 1.0e-3)
            .copied()
            .unwrap_or_else(|| panic!("no hit at y {y}: {all:?}"))
    };
    assert!(by_y(-0.8).normal.y > 0.99, "nested top (entry): {all:?}");
    assert!(by_y(-2.0).normal.y < -0.99, "upper box bottom (exit): {all:?}");
    assert!(by_y(-2.5).normal.y > 0.99, "lower box top (entry): {all:?}");
}
