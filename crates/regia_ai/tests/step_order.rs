// SPDX-License-Identifier: MPL-2.0
//! The fixed-step order of `AiPlugin` (ADR-1892, determinism audit M3).

use regia_ai::{AiPlugin, AiStep};
use regia_app::prelude::*;
use regia_physics::PhysicsStep;

#[derive(Resource, Default)]
struct Order(Vec<&'static str>);

/// `AiStep` runs after `PhysicsStep` with no help from the game, whichever was added first, so an
/// impulse a behavior tree writes lands on the same step on every build.
#[test]
fn ai_runs_after_physics_by_default() {
    let mut app = App::new();
    app.init_resource::<Order>();
    app.add_plugins(AiPlugin);
    app.add_systems(
        FixedUpdate,
        (|mut o: ResMut<Order>| o.0.push("ai")).in_set(AiStep),
    );
    app.add_systems(
        FixedUpdate,
        (|mut o: ResMut<Order>| o.0.push("physics")).in_set(PhysicsStep),
    );
    let step = app.world().resource::<FixedTime>().step();
    app.world_mut().insert_resource(SimStep(step));
    for _ in 0..4 {
        app.update();
    }
    let order = &app.world().resource::<Order>().0;
    assert!(order.len() >= 2, "{order:?}");
    assert!(
        order.chunks(2).all(|c| c.len() < 2 || c == ["physics", "ai"]),
        "{order:?}"
    );
}
