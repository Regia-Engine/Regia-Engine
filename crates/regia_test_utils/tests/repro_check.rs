// SPDX-License-Identifier: MPL-2.0
//! Record/replay determinism stress (ADR-537, `book/agents/determinism.md`): each engine scene is
//! recorded under 20 seeds and replayed from the recording, and the state hashes of the two runs
//! must match at every checkpoint. `samples/arena/tests/repro_check.rs` runs the third scene (the
//! bot match). `cargo xtask repro-check` runs both files.
//!
//! `REGIA_REPRO_SEEDS` (default 20) and `REGIA_REPRO_STEPS` (default 240) size the run;
//! `REGIA_REPRO_REPORT` names a file the markdown report is also written to.
//!
//! The other tests prove the harness itself: it flags a scene that is not a function of its seed
//! and input, flags a replay that loses the input, and does not flag a `HashMap` whose iteration
//! order differs between the runs.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use regia_app::prelude::*;
use regia_test_utils::repro::{
    ReproConfig, ReproScene, character_rig, inject_next_frame, input_noise, physics_pile, record_replay,
    seeds, stress,
};

fn env_number(name: &str, default: u64) -> u64 {
    regia_core::brand::env_var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn config() -> ReproConfig {
    ReproConfig {
        steps: env_number("REGIA_REPRO_STEPS", 240) as u32,
        ..ReproConfig::default()
    }
}

#[test]
fn twenty_seeds_of_each_engine_scene_replay_identically() {
    let seeds = seeds(1, env_number("REGIA_REPRO_SEEDS", 20) as usize);
    let report = stress(&[physics_pile(), character_rig()], &seeds, &config());
    let markdown = report.to_markdown();
    println!("{markdown}");
    if let Some(path) = regia_core::brand::env_var_os("REGIA_REPRO_REPORT") {
        let path = std::path::PathBuf::from(path).with_extension("engine.md");
        regia_test_utils::write_atomic(&path, markdown.as_bytes()).expect("write the repro report");
    }
    assert_eq!(report.runs.len(), seeds.len() * 2);
    assert!(
        report.runs.iter().all(|run| run.values > 20),
        "every run hashed the scene's state: {markdown}"
    );
    assert!(report.is_clean(), "{markdown}");
}

#[test]
fn different_seeds_give_different_states() {
    let config = ReproConfig {
        steps: 60,
        checkpoint_every: 60,
    };
    for scene in [physics_pile(), character_rig()] {
        let a = record_replay(&scene, 1, &config);
        let b = record_replay(&scene, 2, &config);
        assert!(a.error.is_none() && b.error.is_none(), "{a:?} {b:?}");
        assert_ne!(
            a.recorded, b.recorded,
            "{}: the seed reaches the state, so a clean run compares something",
            scene.name
        );
    }
}

// ---------------------------------------------------------------------------
// The harness catches what it should, and only that
// ---------------------------------------------------------------------------

#[derive(Component, Reflect, Default)]
#[reflect(Component)]
struct Tally {
    sum: f32,
    weights: HashMap<String, u32>,
}

/// How many probe apps have been built in this process: a value that is not a function of the seed.
static BUILDS: AtomicU64 = AtomicU64::new(0);

fn accumulate(actions: Res<regia_input::Actions>, mut tallies: Query<&mut Tally>) {
    for mut tally in &mut tallies {
        tally.sum += actions.value("Move").x;
    }
}

/// A one-entity scene: `Tally::sum` integrates the `Move` input. `leak` adds the process build
/// count to the state. The map is a std `HashMap` (a random hasher seed per instance, so the
/// recording's and the replay's maps iterate in different orders), filled in an order that also
/// alternates between builds.
fn probe_app(leak: bool) -> App {
    let build = BUILDS.fetch_add(1, Ordering::Relaxed);
    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins,
        regia_input::InputPlugin::new(
            regia_input::InputConfig::new().action("Move", regia_input::ActionKind::Axis2D),
        ),
        regia_framework::input_recording::InputRecordingPlugin,
    ));
    app.register_type::<Tally>();
    app.add_systems(
        FixedUpdate,
        accumulate.after(regia_framework::input_recording::InputRecordingSet),
    );
    enable_fixed_step(&mut app, 60.0);
    // The same 16 entries every build; only the insertion order alternates.
    let mut entries: Vec<(String, u32)> = (0..16u32).map(|i| (format!("k{i}"), i % 3)).collect();
    if build.is_multiple_of(2) {
        entries.reverse();
    }
    let weights = entries.into_iter().collect();
    let sum = if leak { build as f32 } else { 0.0 };
    app.world_mut().spawn((
        regia_scene::components::Name::new("probe"),
        Tally { sum, weights },
    ));
    app
}

fn probe_drive(app: &mut App, seed: u64, step: u32) {
    let value = regia_core::math::Vec3::new(input_noise(seed, step, 0), 0.0, 0.0);
    inject_next_frame(app, "Move", value);
}

fn probe(name: &'static str, build: fn(u64) -> App) -> ReproScene {
    ReproScene {
        name,
        build,
        actions: &["Move"],
        drive: probe_drive,
        extra: None,
        ignore: &[],
    }
}

const SHORT: ReproConfig = ReproConfig {
    steps: 30,
    checkpoint_every: 10,
};

#[test]
fn a_clean_scene_is_clean_and_its_map_order_does_not_count() {
    // The recording's and the replay's maps hold the same entries in different iteration orders;
    // the state is the same, and so is the hash.
    let run = record_replay(&probe("probe", |_| probe_app(false)), 3, &SHORT);
    assert!(run.is_clean(), "{run:?}");
    assert_eq!(run.recorded.len(), 3, "checkpoints at 10, 20 and 30");
}

#[test]
fn state_that_is_not_a_function_of_seed_and_input_diverges() {
    let run = record_replay(&probe("leaky", |_| probe_app(true)), 3, &SHORT);
    let divergence = run
        .divergence
        .as_ref()
        .expect("the build counter reaches the state");
    assert_eq!(divergence.step, 10, "found at the first checkpoint");
    assert_eq!(divergence.keys, vec!["probe/Tally".to_string()]);
    assert!(!run.is_clean());
}

#[test]
fn a_replay_that_loses_the_input_diverges() {
    // A scene without the recorder cannot be compared: the harness reports it as an error.
    fn no_recorder(_: u64) -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        enable_fixed_step(&mut app, 60.0);
        app
    }
    let run = record_replay(&probe("no_recorder", no_recorder), 3, &SHORT);
    assert!(run.error.is_some(), "{run:?}");

    // The probe scene with `Move` left out of the recording: the replay never sees the drive's
    // input, and the integrated sum differs from the first checkpoint on.
    let untracked = ReproScene {
        actions: &[],
        ..probe("untracked", |_| probe_app(false))
    };
    let run = record_replay(&untracked, 3, &SHORT);
    let divergence = run
        .divergence
        .as_ref()
        .expect("the replay misses the drive's input");
    assert_eq!(divergence.keys, vec!["probe/Tally".to_string()]);
}
