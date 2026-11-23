// SPDX-License-Identifier: MPL-2.0
//! Schedule-shuffle acceptance: the headless smoke scene gives the same observable state under every
//! seeded system order, and the engine's schedules are listed with their ambiguous system pairs
//! (`book/agent-guide/schedule-shuffle.md`). `cargo xtask shuffle-check` runs this file.
//!
//! `REGIA_SHUFFLE_SEEDS` (default 8) and `REGIA_SHUFFLE_FRAMES` (default 90) size the run;
//! `REGIA_SHUFFLE_REPORT` names a file the ambiguity report is also written to.

use regia_test_utils::shuffle::{engine_app, shuffle_check, smoke_app};

fn env_number(name: &str, default: u64) -> u64 {
    regia_core::brand::env_var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[test]
fn the_smoke_scene_is_repeatable_and_has_state() {
    let outcome = shuffle_check(smoke_app, 30, [1]);
    assert!(outcome.repeatable, "{}", outcome.to_markdown());
    assert!(
        outcome.entities >= 12,
        "the scene has named entities: {}",
        outcome.to_markdown()
    );
    assert!(outcome.values > 30, "{}", outcome.to_markdown());
}

#[test]
fn the_smoke_scene_does_not_depend_on_system_order() {
    let seeds = env_number("REGIA_SHUFFLE_SEEDS", 8);
    let frames = env_number("REGIA_SHUFFLE_FRAMES", 90) as u32;
    let outcome = shuffle_check(smoke_app, frames, 1..=seeds);
    println!("{}", outcome.to_markdown());
    assert!(outcome.is_clean(), "{}", outcome.to_markdown());
}

#[test]
fn the_engine_schedules_report_their_ambiguities() {
    let mut app = engine_app();
    let report = app.ambiguity_report();
    let markdown = report.to_markdown(40);
    println!("{markdown}");
    if let Some(path) = regia_core::brand::env_var_os("REGIA_SHUFFLE_REPORT") {
        std::fs::write(&path, report.to_markdown(500)).expect("write the ambiguity report");
    }
    assert!(report.build_errors.is_empty(), "{:?}", report.build_errors);
    for schedule in ["Update", "PostUpdate", "FixedUpdate", "Render", "Extract"] {
        let entry = report
            .schedules
            .iter()
            .find(|s| s.schedule == schedule)
            .unwrap_or_else(|| panic!("{schedule} is built: {markdown}"));
        assert!(
            entry.systems > 0 || schedule == "Extract",
            "{schedule} has systems: {markdown}"
        );
    }
}

#[test]
fn snapshot_keys_survive_duplicate_names_and_spawn_order() {
    use regia_app::App;
    use regia_scene::components::Name;
    use regia_scene::{ScenePlugin, Transform};
    use regia_test_utils::shuffle::snapshot;

    let build = |reverse: bool| {
        let mut app = App::new();
        app.add_plugins(ScenePlugin);
        let at = |x: f32| Transform::from_translation(regia_core::math::Vec3::new(x, 0.0, 0.0));
        let mut xs = [1.0f32, 2.0];
        if reverse {
            xs.reverse();
        }
        for x in xs {
            app.world_mut().spawn((Name::new("hero"), at(x)));
            app.world_mut().spawn(at(x + 10.0));
        }
        snapshot(app.world())
    };
    let forward = build(false);
    assert_eq!(forward, build(true), "keys do not depend on spawn order");
    let heroes = forward.keys().filter(|k| k.starts_with("hero#")).count();
    assert!(
        heroes >= 2,
        "both entities named hero are kept: {:?}",
        forward.keys()
    );
}
