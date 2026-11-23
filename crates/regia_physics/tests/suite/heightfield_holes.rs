// SPDX-License-Identifier: MPL-2.0
//! A terrain hole cuts the heightfield collider (ADR-088): `HolesMask::cut_heights` marks the
//! samples in a tunnel mouth NaN and every cell touching one collides with nothing, so a ray
//! (or a vehicle) goes down the open-cut ramp instead of stopping on ground that is not drawn.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use regia_app::{App, FixedTime, TimeUpdateStrategy};
use regia_core::math::Vec3;
use regia_physics::{Collider, ColliderShape, PhysicsPlugin, PhysicsQuery, QueryFilter, RigidBody};
use regia_scene::Transform;
use regia_terrain::{HoleShape, HolesMask};

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

#[test]
fn rays_fall_through_a_cut_heightfield_and_hit_beside_it() {
    // 41 × 41 samples, 1 m apart, flat at y = 0, centred on the origin.
    let n = 41usize;
    let scale = Vec3::new(40.0, 1.0, 40.0);
    // `heights[row + col * n]`: row runs along z, column along x.
    let xz = |i: usize| {
        let (row, col) = (i % n, i / n);
        [
            (-0.5 + col as f32 / (n - 1) as f32) * scale.x,
            (-0.5 + row as f32 / (n - 1) as f32) * scale.z,
        ]
    };
    let mut heights = vec![0.0f32; n * n];
    // A 10 m × 6 m trench along x through the centre.
    let mask = HolesMask::from_shapes(vec![HoleShape::segment([-5.0, 0.0], [5.0, 0.0], 6.0)]).expect("mask");
    let cut = mask.cut_heights(&mut heights, 0.5, xz);
    assert!(cut > 0);

    let slot: Arc<Mutex<Option<[Option<f32>; 4]>>> = Arc::new(Mutex::new(None));
    let mut app = physics_app();
    app.world_mut().spawn((
        Transform::IDENTITY,
        RigidBody::static_body(),
        Collider {
            shape: ColliderShape::HeightField {
                nx: n,
                nz: n,
                heights,
                scale,
            },
            friction: 0.8,
            ..Collider::default()
        },
    ));
    let slot2 = slot.clone();
    app.add_systems(regia_app::FixedUpdate, move |queries: PhysicsQuery| {
        let down = |x: f32, z: f32| {
            queries
                .raycast(Vec3::new(x, 5.0, z), Vec3::NEG_Y, 20.0, QueryFilter::new())
                .map(|hit| hit.point.y)
        };
        *slot2.lock().expect("slot") =
            Some([down(0.0, 0.0), down(4.0, 2.0), down(0.0, 8.0), down(12.0, 0.0)]);
    });
    app.update();
    app.update();

    let [centre, inside, beside, past_end] = slot.lock().expect("slot").take().expect("system ran");
    assert!(centre.is_none(), "a ray down the trench center hit {centre:?}");
    assert!(inside.is_none(), "a ray down the trench hit {inside:?}");
    assert!(
        beside.is_some_and(|y| y.abs() < 0.01),
        "ground beside the trench: {beside:?}"
    );
    assert!(
        past_end.is_some_and(|y| y.abs() < 0.01),
        "ground past its end: {past_end:?}"
    );
}

#[test]
fn non_square_heightfield_nx_is_the_row_count_along_z() {
    // 5 rows along Z, 3 columns along X: parry reports cells (one fewer than samples).
    let shape = ColliderShape::HeightField {
        nx: 5,
        nz: 3,
        heights: vec![0.0; 15],
        scale: Vec3::new(20.0, 1.0, 40.0),
    };
    let shared = regia_physics::step::shape_to_shared_shape(&shape).expect("valid heightfield");
    let field = shared.as_heightfield().expect("heightfield shape");
    assert_eq!(field.nrows(), 4, "nx samples run along Z");
    assert_eq!(field.ncols(), 2, "nz samples run along X");
}
