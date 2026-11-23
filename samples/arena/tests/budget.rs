// SPDX-License-Identifier: MIT-0
//! Budget-probe counts (`docs/specs/14` M20): 8 bots, at least 200 rigid bodies and at least
//! 20,000 live particles. Frame timings are printed by `cargo run -p arena -- --bench`.

use regia_app::{App, MinimalPlugins, enable_fixed_step};
use regia_audio::AudioSettings;
use regia_framework::settings::SettingsPath;

use arena::budget::{BUDGET_BODIES, BUDGET_BOTS, BUDGET_PARTICLES};
use arena::{ArenaConfig, ArenaSample, MatchRng};

#[test]
fn budget_load_reaches_bots_bodies_and_particles() {
    let mut app = App::new();
    app.insert_resource(AudioSettings {
        headless: true,
        ..Default::default()
    });
    app.insert_resource(SettingsPath(None));
    app.add_plugins((MinimalPlugins, ArenaSample));
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::CursorMoved>()
        .add_message::<regia_platform::MouseButtonInput>()
        .add_message::<regia_platform::TouchInput>()
        .add_message::<regia_platform::MouseWheel>()
        .add_message::<regia_platform::KeyboardInput>()
        .add_message::<regia_platform::ImeEvent>();
    app.insert_resource(ArenaConfig {
        bot_count: BUDGET_BOTS,
        local_player: false,
        score_limit: 1_000_000,
        warmup_secs: 3.0,
        match_secs: 180.0,
        use_navmesh: false,
        brain: arena::BotBrainKind::Steering,
    });
    app.insert_resource(MatchRng::new(1));
    enable_fixed_step(&mut app, 60.0);
    app.update();
    arena::budget::apply_budget_load(app.world_mut());
    for _ in 0..3 {
        app.update();
    }
    let counts = arena::budget::budget_counts(app.world_mut());
    assert!(
        counts.bots >= BUDGET_BOTS,
        "bots {} (want {BUDGET_BOTS})",
        counts.bots
    );
    assert!(
        counts.bodies >= BUDGET_BODIES,
        "rigid bodies {} (want {BUDGET_BODIES})",
        counts.bodies
    );
    assert!(
        counts.particles >= BUDGET_PARTICLES,
        "live particles {} (want {BUDGET_PARTICLES})",
        counts.particles
    );
}
