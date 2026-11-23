// SPDX-License-Identifier: MPL-2.0
//! Review proof for the foot-placement ground ray.
//!
//! A solid cast that starts inside a shape reports time of impact 0 at the ray origin
//! (parry's support-map cast: the simplex distance is already zero, so it returns
//! immediately). `PhysicsGroundProbe` forwards that hit. Foot placement then plants on
//! the ray origin, `probe_height` above the foot, still inside the volume.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_physics::{
    Collider, ColliderShape, PhysicsPlugin, PhysicsQuery, PhysicsStep, QueryFilter, RigidBody,
};

#[test]
fn review_ray_inside_a_box_is_not_the_origin() {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    let mut fixed = regia_app::FixedTime::default();
    fixed.hz = 60.0;
    app.insert_resource(fixed);
    app.add_plugins(PhysicsPlugin);
    // Top face at y = 0, bottom face at y = -2. The ray starts inside the box.
    app.world_mut().spawn((
        regia_scene::Transform::from_translation(Vec3::new(0.0, -1.0, 0.0)),
        RigidBody::static_body(),
        Collider {
            shape: ColliderShape::Box {
                half_extents: Vec3::new(2.0, 1.0, 2.0),
            },
            ..Collider::default()
        },
    ));

    let slot = Arc::new(Mutex::new(None));
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

    let hit = slot
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .expect("a ray that starts inside the ground still has to find a face");
    assert!(
        hit.distance > 1.0e-3,
        "solid ray inside the ground reported toi {} at {:?} (the ray origin). \
         Foot placement would plant the sole there",
        hit.distance,
        hit.point
    );
    let on_face = (hit.point.y - 0.0).abs() < 1.0e-2 || (hit.point.y + 2.0).abs() < 1.0e-2;
    assert!(
        on_face,
        "hit {:?} is not on the box (top y = 0, bottom y = -2)",
        hit.point
    );
}
