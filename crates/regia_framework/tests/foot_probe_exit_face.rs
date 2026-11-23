// SPDX-License-Identifier: MPL-2.0
//! ADR-8801, end to end: the physics ground probe foot placement uses (`PhysicsGroundProbe`
//! through `GroundProbes`) never reports the underside of a slab its ray started inside as
//! ground. The ray reports the face it leaves by with that face's outward normal (along the
//! ray), and `GroundProbes` drops it as a back face. A probe from above still finds the top.
//!
//! Its own test binary: the probe reads a process-wide pointer published for one schedule
//! window, so no other test may publish or clear it while this one runs.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use regia_animation::foot_ik::{GroundHit, GroundProbes};
use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_framework::foot_probe::{PhysicsGroundProbe, clear_physics_probe, publish_physics_probe};
use regia_physics::{Collider, ColliderShape, PhysicsPlugin, RigidBody};

#[test]
fn a_probe_inside_a_slab_finds_no_ground_on_its_underside() {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    let mut fixed = regia_app::FixedTime::default();
    fixed.hz = 60.0;
    app.insert_resource(fixed);
    app.add_plugins(PhysicsPlugin);
    app.insert_resource(GroundProbes::new(PhysicsGroundProbe));
    // A slab: top face y = 0, bottom face y = -2.
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
    type Out = (Option<GroundHit>, Option<GroundHit>);
    let slot: Arc<Mutex<Option<Out>>> = Arc::new(Mutex::new(None));
    let out = slot.clone();
    let probe = move |probes: Res<GroundProbes>| {
        let ignore = Entity::PLACEHOLDER;
        let inside = probes.probe(Vec3::new(0.0, -0.4, 0.0), Vec3::NEG_Y, 3.0, ignore);
        let above = probes.probe(Vec3::new(0.0, 0.6, 0.0), Vec3::NEG_Y, 3.0, ignore);
        *out.lock().unwrap_or_else(|p| p.into_inner()) = Some((inside, above));
    };
    app.add_systems(
        Update,
        (publish_physics_probe, probe, clear_physics_probe).chain(),
    );
    for _ in 0..6 {
        app.update();
    }
    let (inside, above) = slot
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .expect("the probe system ran");
    assert!(
        inside.is_none(),
        "a probe that starts inside the slab planted on its underside: {inside:?}"
    );
    let above = above.expect("a probe from above finds the top face");
    assert!(above.point.y.abs() < 1.0e-3 && above.normal.y > 0.99, "{above:?}");
}
