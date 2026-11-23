// SPDX-License-Identifier: MIT-0
//! End-to-end navmesh pathing for the arena (specs/22 § T2.2): with `ArenaConfig.use_navmesh`
//! set, the startup builds a `NavMesh` from the static colliders and attaches a `NavAgent`, a
//! behaviour tree and a blackboard to every bot; the bots then navigate the mesh instead of
//! steering.

use regia_app::prelude::Entity;
use regia_app::{App, MinimalPlugins, enable_fixed_step};
use regia_audio::AudioSettings;
use regia_core::math::Vec3;
use regia_framework::settings::SettingsPath;
use regia_scene::Transform;

use arena::{ArenaConfig, ArenaSample, MatchRng};

/// A headless arena app with navmesh pathing on.
fn nav_app() -> App {
    let mut app = App::new();
    app.insert_resource(AudioSettings {
        headless: true,
        ..Default::default()
    });
    app.insert_resource(SettingsPath(None));
    // `use_navmesh` must be set before `ArenaSample` is added (the sample reads it at build time).
    app.insert_resource(ArenaConfig {
        bot_count: 3,
        local_player: false,
        score_limit: 1_000_000,
        warmup_secs: 0.0,
        match_secs: 180.0,
        use_navmesh: true,
        brain: arena::BotBrainKind::Steering,
    });
    app.insert_resource(MatchRng::new(7));
    app.add_plugins((MinimalPlugins, ArenaSample));
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::CursorMoved>()
        .add_message::<regia_platform::MouseButtonInput>()
        .add_message::<regia_platform::TouchInput>()
        .add_message::<regia_platform::MouseWheel>()
        .add_message::<regia_platform::KeyboardInput>()
        .add_message::<regia_platform::ImeEvent>();
    enable_fixed_step(&mut app, 60.0);
    app
}

/// The XZ positions of all bots, in entity order.
fn bot_positions(app: &mut App) -> Vec<(Vec3, Entity)> {
    let mut out = Vec::new();
    let mut query = app
        .world_mut()
        .query::<(Entity, &arena::pawn::Pawn, &Transform)>();
    for (entity, pawn, transform) in query.iter(app.world()) {
        if pawn.is_bot {
            out.push((transform.translation, entity));
        }
    }
    out
}

#[test]
fn bots_navigate_on_the_navmesh_and_run_a_behaviour_tree() {
    let mut app = nav_app();
    app.update(); // Startup builds the world, the navmesh and the bot components.

    // The navmesh exists and has a single walkable floor region.
    let mesh = app.world().resource::<regia_nav::NavMesh>();
    assert!(mesh.poly_count() > 0, "a navmesh was built");
    assert!(mesh.region_count >= 1);

    // Every bot has a NavAgent, a blackboard and a compiled behaviour tree.
    for (_, entity) in bot_positions(&mut app) {
        assert!(
            app.world().get::<regia_nav::NavAgent>(entity).is_some(),
            "bot has a NavAgent"
        );
        assert!(
            app.world().get::<regia_ai::Blackboard>(entity).is_some(),
            "bot has a blackboard"
        );
        assert!(
            app.world().get::<regia_ai::BehaviorTree>(entity).is_some(),
            "bot has a behavior tree"
        );
    }

    // The bots path toward each other and move off their spawn points.
    let before: Vec<Vec3> = bot_positions(&mut app).iter().map(|(p, _)| *p).collect();
    for _ in 0..120 {
        app.update();
    }
    let after: Vec<Vec3> = bot_positions(&mut app).iter().map(|(p, _)| *p).collect();
    assert_eq!(before.len(), after.len());
    assert!(before.len() >= 3, "three bots spawned");
    let moved = before
        .iter()
        .zip(after.iter())
        .any(|(a, b)| (a - *b).length() > 0.1);
    assert!(moved, "the bots moved along the navmesh");
}
