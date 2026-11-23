// SPDX-License-Identifier: MPL-2.0
//! Replay soak acceptance (ADR-867, `book/agents/replay-soak.md`): every built-in scene records a
//! seeded input stream and replays it to the same state hash at every checkpoint, and the
//! comparison reports a planted divergence with the step and the component that left.
//!
//! `cargo xtask replay-soak` runs `replay_soak_matrix` once per compute-thread count and build
//! profile, sized through the environment:
//!
//! | Variable | Default (gate) | Meaning |
//! |---|---|---|
//! | `REGIA_REPLAY_SOAK_OUT` | unset | directory for `run-<label>.json`; set = soak mode |
//! | `REGIA_REPLAY_SOAK_SCENES` | all | comma-separated scene names |
//! | `REGIA_REPLAY_SOAK_SEEDS` | 2 (soak: 8) | seeds per scene |
//! | `REGIA_REPLAY_SOAK_FIRST_SEED` | 1 | first seed |
//! | `REGIA_REPLAY_SOAK_STEPS` | 120 (soak: 600) | fixed steps per run |
//! | `REGIA_REPLAY_SOAK_EVERY` | 20 (soak: 10) | steps between checkpoints |
//! | `REGIA_REPLAY_SOAK_THREADS` | unset | compute threads (unset: the default formula) |
//! | `REGIA_REPLAY_SOAK_LABEL` | `local` | run label |
//! | `REGIA_REPLAY_SOAK_BUILD` | `debug`/`release` | build profile name for the report |

use std::sync::atomic::{AtomicU32, Ordering};

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_framework::input_recording::InputRecordingSet;
use regia_scene::components::Name;
use regia_test_utils::replay_soak::{
    RunReport, SCENES, SoakConfig, SoakScene, combat_scene, init_threads, record, replay, scene, soak_seed,
};

fn env_or<T: std::str::FromStr>(name: &str, default: T) -> T {
    regia_core::brand::env_var(name)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

#[test]
fn replay_soak_matrix() {
    // Before any app exists: only the first sizing in a process takes effect.
    let requested: Option<usize> = regia_core::brand::env_var("REGIA_REPLAY_SOAK_THREADS")
        .ok()
        .and_then(|v| v.trim().parse().ok());
    let threads = init_threads(requested);
    let out = regia_core::brand::env_var_os("REGIA_REPLAY_SOAK_OUT").map(std::path::PathBuf::from);
    let soak = out.is_some();
    let seeds: u64 = env_or("REGIA_REPLAY_SOAK_SEEDS", if soak { 8 } else { 2 });
    let first: u64 = env_or("REGIA_REPLAY_SOAK_FIRST_SEED", 1);
    let config = SoakConfig {
        steps: env_or("REGIA_REPLAY_SOAK_STEPS", if soak { 600 } else { 120 }),
        every: env_or("REGIA_REPLAY_SOAK_EVERY", if soak { 10 } else { 20 }),
    };
    let scenes: Vec<SoakScene> = match regia_core::brand::env_var("REGIA_REPLAY_SOAK_SCENES") {
        Ok(list) if !list.trim().is_empty() => list
            .split(',')
            .map(|name| scene(name.trim()).unwrap_or_else(|| panic!("unknown replay-soak scene {name:?}")))
            .collect(),
        _ => SCENES.to_vec(),
    };
    let label = regia_core::brand::env_var("REGIA_REPLAY_SOAK_LABEL").unwrap_or_else(|_| "local".into());
    let build = regia_core::brand::env_var("REGIA_REPLAY_SOAK_BUILD").unwrap_or_else(|_| {
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
        .to_string()
    });

    let report = RunReport::run(
        &scenes,
        first..first + seeds,
        config,
        label.clone(),
        build,
        requested,
        threads,
    );
    println!("{}", report.to_markdown());
    if let Some(dir) = out {
        std::fs::create_dir_all(&dir).expect("create the replay-soak output directory");
        let path = dir.join(format!("run-{label}.json"));
        regia_test_utils::write_atomic(&path, report.to_json().as_bytes())
            .expect("write the replay-soak run report");
        println!("replay-soak: wrote {}", path.display());
    }
    assert!(report.is_clean(), "{}", report.to_markdown());
    for run in &report.runs {
        assert!(
            run.active_inputs > 0,
            "{} seed {} had no input",
            run.scene,
            run.seed
        );
    }
}

#[test]
fn every_scene_has_state_that_moves_and_follows_its_input() {
    let config = SoakConfig { steps: 90, every: 30 };
    for scene in SCENES {
        let a = soak_seed(&scene, 1, &config);
        assert!(
            a.replay_matches(),
            "{} replays: {:?}",
            scene.name,
            a.replay_divergence
        );
        assert!(a.entities >= 5, "{} has entities: {}", scene.name, a.entities);
        assert!(a.values >= 10, "{} has values: {}", scene.name, a.values);
        let hashes: Vec<u64> = a.record.iter().map(|c| c.hash).collect();
        assert_eq!(hashes.len(), 3, "{}: checkpoints at 30, 60, 90", scene.name);
        assert!(
            hashes.windows(2).all(|w| w[0] != w[1]),
            "{}: the state changes between checkpoints: {hashes:x?}",
            scene.name
        );
        let b = soak_seed(&scene, 2, &config);
        assert_ne!(
            a.record.last().map(|c| c.hash),
            b.record.last().map(|c| c.hash),
            "{}: another seed's input stream gives another state",
            scene.name
        );
    }
}

#[test]
fn the_crowd_scene_has_a_navmesh_and_its_agents_walk_their_plans() {
    let crowd = scene("crowd").expect("crowd scene");
    let app = (crowd.build)(4);
    assert!(
        app.world().contains_resource::<regia_nav::NavMesh>(),
        "the floor builds a navmesh"
    );
    let before = regia_test_utils::shuffle::snapshot(app.world());
    let recorded = record(
        &crowd,
        4,
        &SoakConfig {
            steps: 240,
            every: 240,
        },
    );
    let after = recorded.snapshots.last().expect("a final snapshot");
    let plans = after
        .get("rally/SoakRally")
        .expect("the rally state is in the snapshot");
    assert!(!plans.contains("plans: 0 }"), "the input planned paths: {plans}");
    let agents: Vec<&String> = before
        .keys()
        .filter(|k| k.starts_with("agent") && k.ends_with("/Transform"))
        .collect();
    assert_eq!(agents.len(), 12, "twelve named agents: {agents:?}");
    let moved = agents
        .iter()
        .filter(|key| after.get(key.as_str()) != before.get(key.as_str()))
        .count();
    assert!(moved >= 6, "most agents walked: {moved} of 12");
}

#[test]
fn a_one_step_input_lag_is_reported_as_a_divergence() {
    // The pre-ADR-867 playback bug: every recorded step played one step late.
    let smoke = scene("smoke").expect("smoke scene");
    let config = SoakConfig { steps: 60, every: 5 };
    let recorded = record(&smoke, 7, &config);
    let mut late = recorded.recording.clone();
    let idle = vec![Vec3::ZERO; late.actions.len()];
    late.frames.insert(0, idle);
    late.frames.pop();
    late.timestamps.clear();
    let replayed = replay(&smoke, &late, &recorded, &config);
    let divergence = replayed
        .divergence
        .expect("a replay one step late leaves the recording");
    assert!(
        !divergence.differing.is_empty(),
        "the divergence names the values that left: {divergence:?}"
    );
    // The faithful recording replays cleanly.
    let clean = replay(&smoke, &recorded.recording, &recorded, &config);
    assert_eq!(clean.divergence, None);
}

/// A value that differs on every app the process builds: a planted nondeterminism.
static BUILDS: AtomicU32 = AtomicU32::new(0);

/// Counts steps from a start that depends on how many apps the process built before.
#[derive(Component, Reflect, Debug, Clone, Default, PartialEq)]
#[reflect(Component, Default)]
struct Leak(u32);

fn leak_step(mut leaks: Query<&mut Leak>) {
    for mut leak in &mut leaks {
        leak.0 = leak.0.wrapping_add(1);
    }
}

fn leaky_combat(seed: u64) -> App {
    let mut app = combat_scene(seed);
    app.register_type::<Leak>();
    app.world_mut().spawn((
        Name::new("leak"),
        Leak(BUILDS.fetch_add(1, Ordering::Relaxed).wrapping_mul(1000)),
    ));
    app.add_systems(FixedUpdate, leak_step.after(InputRecordingSet));
    app
}

#[test]
fn a_planted_nondeterminism_is_reported_with_its_component() {
    let leaky = SoakScene {
        name: "leaky_combat",
        build: leaky_combat,
        actions: SCENES[2].actions,
        components: &[],
    };
    let report = soak_seed(&leaky, 3, &SoakConfig { steps: 30, every: 10 });
    let divergence = report
        .replay_divergence
        .expect("the planted per-build value is caught");
    assert_eq!(divergence.step, 10, "caught at the first checkpoint");
    let leak = divergence
        .differing
        .iter()
        .find(|d| d.key == "leak/Leak")
        .unwrap_or_else(|| panic!("the planted component is named: {divergence:?}"));
    assert_ne!(leak.record, leak.replay);
    assert!(
        divergence.differing.iter().all(|d| d.key.starts_with("leak/")),
        "nothing else differs: {divergence:?}"
    );
}

#[test]
fn the_run_report_json_carries_every_documented_field() {
    let combat = scene("combat").expect("combat scene");
    let report = RunReport::run(
        &[combat],
        [5],
        SoakConfig { steps: 20, every: 10 },
        "debug-t2",
        "debug",
        Some(2),
        2,
    );
    let json = report.to_json();
    for field in [
        "\"format\": \"regia-replay-soak-run\"",
        "\"version\": 1",
        "\"label\": \"debug-t2\"",
        "\"build\": \"debug\"",
        "\"threads_requested\": 2",
        "\"threads\": 2",
        "\"os\": ",
        "\"arch\": ",
        "\"steps\": 20",
        "\"every\": 10",
        "\"scene\": \"combat\"",
        "\"seed\": 5",
        "\"active_inputs\": ",
        "\"record\": [{\"step\": 10, \"hash\": \"",
        "\"entities\": {",
        "\"turret\": \"",
        "\"replay\": [{\"step\": 10, \"hash\": \"",
        "\"replay_divergence\": null",
    ] {
        assert!(json.contains(field), "missing {field} in\n{json}");
    }
    assert_eq!(
        json.matches('{').count(),
        json.matches('}').count(),
        "balanced braces"
    );
}
