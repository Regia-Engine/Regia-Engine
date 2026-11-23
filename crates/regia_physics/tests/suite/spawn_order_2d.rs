// SPDX-License-Identifier: MPL-2.0
//! 2D bodies enter rapier in the order they arrive in the world, not in entity-id order
//! (ADR-1347; see `spawn_order.rs` for the 3D side and why ids are not repeatable).
#![cfg(feature = "physics2d")]

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_ecs::ecs::world::CommandQueue;
use regia_physics::physics2d::{Body2dHandle, Collider2d, Physics2dPlugin, RigidBody2d};
use regia_scene::Transform;

fn app() -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, Physics2dPlugin));
    enable_fixed_step(&mut app, 60.0);
    app.world_mut().spawn((
        Collider2d::cuboid(40.0, 2.0),
        Transform::from_translation(Vec3::new(0.0, -1.0, 0.0)),
    ));
    app
}

fn falling_box(x: f32, y: f32) -> impl Bundle {
    (
        RigidBody2d::default(),
        Collider2d::cuboid(1.0, 1.0),
        Transform::from_translation(Vec3::new(x, y, 0.0)),
    )
}

#[test]
fn bodies_2d_enter_rapier_in_arrival_order() {
    let mut app = app();
    // The second queue reserves its id first (it "ran first" on another thread) but is applied
    // second (system order).
    let mut first = CommandQueue::default();
    let mut second = CommandQueue::default();
    let (late, early) = {
        let world = app.world();
        let late = Commands::new(&mut second, world)
            .spawn(falling_box(0.3, 3.0))
            .id();
        let early = Commands::new(&mut first, world).spawn(falling_box(0.0, 1.0)).id();
        (late, early)
    };
    first.apply(app.world_mut());
    second.apply(app.world_mut());
    assert!(
        late.index().index() < early.index().index(),
        "the ids are inverted"
    );
    app.update();
    let raw = |entity: Entity| {
        app.world()
            .get::<Body2dHandle>(entity)
            .map(|h| h.0.into_raw_parts().0)
            .unwrap_or_else(|| panic!("{entity:?} has a rapier body"))
    };
    assert!(
        raw(early) < raw(late),
        "the body applied first gets the lower rapier handle"
    );
}
