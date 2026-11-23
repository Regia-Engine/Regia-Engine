// SPDX-License-Identifier: MIT-0
//! Record/replay determinism stress, third scene (ADR-537, `book/agents/determinism.md`): the bot
//! match with a local player driven by seeded input, recorded under 20 seeds and replayed from the
//! recording. Every checkpoint's state hash (every reflected component, plus the match RNG) must
//! match. `arena_match` runs the steering bots (hitscan, rockets, props, scoring); `arena_navmesh`
//! the behaviour-tree bots on the navmesh (`regia_ai` + `regia_nav`); `arena_state_tree` the default
//! state-tree bots with abilities, the weapon chooser and resupply stations (ADR-2134).
//!
//! `REGIA_REPRO_SEEDS` (default 20) and `REGIA_REPRO_STEPS` (default 240) size the run;
//! `REGIA_REPRO_REPORT` names a file the markdown report is also written to (with the extension
//! `arena.md`). `cargo xtask repro-check` runs this file and `regia_test_utils`'s.

use regia_app::prelude::*;
use regia_audio::AudioSettings;
use regia_core::math::Vec3;
use regia_framework::settings::SettingsPath;
use regia_test_utils::repro::{
    ReproConfig, ReproScene, inject_next_frame, input_noise, record_replay, seeds, stress,
};
use regia_test_utils::shuffle::Snapshot;

use arena::{ArenaConfig, ArenaSample, BotBrainKind, MatchRng};

fn env_number(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// A headless arena: no warm-up, a score limit the match never reaches, six bots and the local
/// player. `ArenaConfig` goes in before `ArenaSample` (the sample reads `use_navmesh` at build).
fn arena_app(seed: u64, use_navmesh: bool, brain: BotBrainKind) -> App {
    let mut app = App::new();
    app.insert_resource(AudioSettings {
        headless: true,
        ..Default::default()
    });
    app.insert_resource(SettingsPath(None));
    app.insert_resource(ArenaConfig {
        bot_count: 6,
        local_player: true,
        score_limit: 1_000_000,
        warmup_secs: 0.0,
        match_secs: 600.0,
        use_navmesh,
        brain,
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

/// The local player: a new heading every 15 steps, a small look sweep every step, a shot every
/// 25 steps and a jump every 70 (phases from the seed).
fn drive_player(app: &mut App, seed: u64, step: u32) {
    let leg = step / 15;
    inject_next_frame(
        app,
        "Move",
        Vec3::new(input_noise(seed, leg, 0), input_noise(seed, leg, 1), 0.0),
    );
    inject_next_frame(
        app,
        "Look",
        Vec3::new(input_noise(seed, step, 2) * 4.0, input_noise(seed, step, 3), 0.0),
    );
    let phase = ((input_noise(seed, 0, 4) * 0.5 + 0.5) * 24.0) as u32;
    if (step + phase).is_multiple_of(25) {
        inject_next_frame(app, "Fire", Vec3::X);
    }
    if (step + phase).is_multiple_of(70) {
        inject_next_frame(app, "Jump", Vec3::X);
    }
}

/// The match RNG is simulation state outside any component.
fn match_rng(world: &World, snapshot: &mut Snapshot) {
    if let Some(rng) = world.get_resource::<MatchRng>() {
        snapshot.insert("$resource/MatchRng".into(), format!("{rng:?}"));
    }
}

const ACTIONS: &[&str] = &["Fire", "Jump", "Look", "Move"];

fn arena_match() -> ReproScene {
    ReproScene {
        name: "arena_match",
        build: |seed| arena_app(seed, false, BotBrainKind::Steering),
        actions: ACTIONS,
        drive: drive_player,
        extra: Some(match_rng),
        // Random by design (`EntityGuid::new_random`), not simulation state.
        ignore: &["EntityGuid"],
    }
}

fn arena_navmesh() -> ReproScene {
    ReproScene {
        name: "arena_navmesh",
        build: |seed| arena_app(seed, true, BotBrainKind::Steering),
        ..arena_match()
    }
}

/// The default brain (ADR-2134): state-tree bots firing through abilities picked by the weapon
/// chooser, restocking at smart-object stations.
fn arena_state_tree() -> ReproScene {
    ReproScene {
        name: "arena_state_tree",
        build: |seed| arena_app(seed, false, BotBrainKind::StateTree),
        ..arena_match()
    }
}

#[test]
fn twenty_seeds_of_the_bot_match_replay_identically() {
    let seeds = seeds(1, env_number("REGIA_REPRO_SEEDS", 20) as usize);
    let config = ReproConfig {
        steps: env_number("REGIA_REPRO_STEPS", 240) as u32,
        ..ReproConfig::default()
    };
    let report = stress(
        &[arena_match(), arena_navmesh(), arena_state_tree()],
        &seeds,
        &config,
    );
    let markdown = report.to_markdown();
    println!("{markdown}");
    if let Some(path) = std::env::var_os("REGIA_REPRO_REPORT") {
        let path = std::path::PathBuf::from(path).with_extension("arena.md");
        regia_test_utils::write_atomic(&path, markdown.as_bytes()).expect("write the repro report");
    }
    assert_eq!(report.runs.len(), seeds.len() * 3);
    assert!(
        report.runs.iter().all(|run| run.values > 20),
        "every run hashed the match state: {markdown}"
    );
    assert!(report.is_clean(), "{markdown}");
}

#[test]
fn the_seed_and_the_input_reach_the_match_state() {
    let config = ReproConfig {
        steps: 90,
        checkpoint_every: 90,
    };
    let a = record_replay(&arena_match(), 1, &config);
    let b = record_replay(&arena_match(), 2, &config);
    assert!(a.error.is_none() && b.error.is_none(), "{a:?} {b:?}");
    assert_ne!(
        a.recorded, b.recorded,
        "a clean run compares state that depends on the seed"
    );
}
