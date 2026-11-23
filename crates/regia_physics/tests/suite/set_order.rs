// SPDX-License-Identifier: MPL-2.0
//! The 3D, 2D and platformer fixed-step chains run in one fixed order, whatever order the
//! plugins and gameplay systems were registered in, so an entity driven by more than one of
//! them always ends the step with the same `Transform` writer last.
#![cfg(feature = "physics2d")]

use regia_app::prelude::*;
use regia_physics::physics2d::{Physics2dPlugin, Physics2dStep};
use regia_physics::platformer2d::{PlatformerPlugin, PlatformerStep};
use regia_physics::{PhysicsPlugin, PhysicsStep};

#[derive(Resource, Default)]
struct Order(Vec<&'static str>);

fn probe(tag: &'static str) -> impl FnMut(ResMut<Order>) {
    move |mut order| order.0.push(tag)
}

#[test]
fn physics_physics2d_platformer_sets_run_in_a_fixed_order() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    enable_fixed_step(&mut app, 60.0);
    app.init_resource::<Order>();
    // Plugins and probes registered in the reverse of the expected order.
    app.add_plugins((PlatformerPlugin, Physics2dPlugin, PhysicsPlugin));
    app.add_systems(FixedUpdate, probe("platformer").in_set(PlatformerStep));
    app.add_systems(FixedUpdate, probe("physics2d").in_set(Physics2dStep));
    app.add_systems(FixedUpdate, probe("physics3d").in_set(PhysicsStep));
    for _ in 0..4 {
        app.update();
    }
    let order = &app.world().resource::<Order>().0;
    assert!(!order.is_empty(), "the fixed step ran");
    let (steps, rest) = order.as_chunks::<3>();
    assert!(rest.is_empty(), "each probe ran once per step: {order:?}");
    for step in steps {
        assert_eq!(step, &["physics3d", "physics2d", "platformer"]);
    }
}
