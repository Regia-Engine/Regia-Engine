// SPDX-License-Identifier: MIT-0
//! Replay soak of the arena with a local player (ADR-867, `book/agents/replay-soak.md`): a seeded
//! input stream drives the player's `Move`, `Look`, `Jump` and `Fire` while bots fight, and the
//! replay of the recording matches it at every checkpoint, gameplay component by component.
//!
//! `determinism.rs` replays a bot-only match and compares the final scores. Here the recorded
//! input steers the player as well, which is what a one-step playback lag would break.

use regia_app::{App, MinimalPlugins, enable_fixed_step};
use regia_audio::AudioSettings;
use regia_framework::settings::SettingsPath;
use regia_input::ActionKind;
use regia_test_utils::replay_soak::{SoakConfig, SoakScene, soak_seed};

use arena::{ArenaConfig, ArenaSample, MatchRng};

/// A headless arena with a local player, seeded like `determinism.rs`. With `use_navmesh` the bots
/// are behaviour trees following navmesh paths (`navmesh.rs`) instead of steering.
fn arena_with(seed: u64, use_navmesh: bool) -> App {
    let mut app = App::new();
    app.insert_resource(AudioSettings {
        headless: true,
        ..Default::default()
    });
    app.insert_resource(SettingsPath(None));
    // Before `ArenaSample`: it reads `use_navmesh` when it is built.
    app.insert_resource(ArenaConfig {
        bot_count: if use_navmesh { 4 } else { 6 },
        local_player: true,
        score_limit: 1_000_000,
        warmup_secs: 0.0,
        match_secs: 600.0,
        use_navmesh,
        brain: arena::BotBrainKind::Steering,
    });
    app.insert_resource(MatchRng::new(seed));
    app.add_plugins((MinimalPlugins, ArenaSample));
    // The UI systems read the platform input messages; register them (no window in a test).
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

fn arena_app(seed: u64) -> App {
    arena_with(seed, false)
}

fn arena_nav_app(seed: u64) -> App {
    arena_with(seed, true)
}

/// The recorded actions: everything the local player reads.
const ACTIONS: &[(&str, ActionKind)] = &[
    ("Fire", ActionKind::Button),
    ("Jump", ActionKind::Button),
    ("Look", ActionKind::Axis2D),
    ("Move", ActionKind::Axis2D),
];

/// The arena as a soak scene. The state hash covers the gameplay components only: the arena also
/// holds mesh, material and audio handles whose ids come from process-wide counters, so two apps in
/// one process differ there whatever the input.
const ARENA: SoakScene = SoakScene {
    name: "arena",
    build: arena_app,
    actions: ACTIONS,
    components: &[
        "ArenaState",
        "BotBrain",
        "Died",
        "Health",
        "Pawn",
        "Projectile",
        "Prop",
        "Transform",
        "Velocity",
    ],
};

#[test]
fn the_arena_with_a_local_player_replays_its_recording_at_every_checkpoint() {
    let config = SoakConfig {
        steps: 600,
        every: 30,
    };
    for seed in [11, 12] {
        let report = soak_seed(&ARENA, seed, &config);
        assert!(
            report.replay_matches(),
            "seed {seed}: the replay left the recording: {:#?}",
            report.replay_divergence
        );
        assert!(report.active_inputs > 100, "seed {seed}: the player was driven");
        assert!(
            report.entities >= 8,
            "seed {seed}: pawns and props are hashed: {}",
            report.entities
        );
    }
}

/// The navmesh arena: behaviour-tree bots plan paths that the nav step follows (`AiStep` then
/// `NavStep` in the arena's fixed-step chain). Agent plans are hashed too.
const ARENA_NAV: SoakScene = SoakScene {
    name: "arena_nav",
    build: arena_nav_app,
    actions: ACTIONS,
    components: &[
        "ArenaState",
        "Died",
        "Health",
        "NavAgentState",
        "Pawn",
        "Projectile",
        "Prop",
        "Transform",
        "Velocity",
    ],
};

#[test]
fn the_navmesh_arena_replays_its_recording_at_every_checkpoint() {
    let report = soak_seed(
        &ARENA_NAV,
        21,
        &SoakConfig {
            steps: 600,
            every: 30,
        },
    );
    assert!(
        report.replay_matches(),
        "the replay left the recording: {:#?}",
        report.replay_divergence
    );
    assert!(report.entities >= 6, "pawns are hashed: {}", report.entities);
}
