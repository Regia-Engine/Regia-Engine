// SPDX-License-Identifier: MIT-0
//! The arena's smoke test (spec 41 test 19, ADR-2755) runs in process: `arena::smoke::install` places
//! `arena_smoke`, the runner picks it by the `smoke` tag, and the report passes. This process uses
//! `MinimalPlugins` and opens no window. The session (`smoke.session.ron`, `regia test session`)
//! runs the same test in the real binary, which opens a window.
#![cfg(feature = "agent")]

use regia_app::{App, MinimalPlugins, enable_fixed_step};
use regia_audio::AudioSettings;
use regia_framework::functional::report::EXIT_PASSED;
use regia_framework::functional::{RunOptions, Runner, RunnerStatus, Selection};
use regia_framework::settings::SettingsPath;

use arena::{ArenaConfig, ArenaSample, MatchRng};

fn app_with(install: fn(&mut App)) -> App {
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
        bot_count: 3,
        local_player: false,
        use_navmesh: false,
        ..ArenaConfig::default()
    });
    app.insert_resource(MatchRng::new(7));
    enable_fixed_step(&mut app, 60.0);
    install(&mut app);
    app
}

fn app() -> App {
    app_with(arena::smoke::install)
}

fn selection() -> Selection {
    Selection {
        name_globs: Vec::new(),
        tags: vec!["smoke".into()],
    }
}

#[test]
fn the_smoke_test_passes_on_the_arena_match() {
    let mut app = app();
    // One frame runs Startup (the arena world and the placed test).
    app.update();
    app.world_mut()
        .resource_mut::<Runner>()
        .start(selection(), RunOptions::default());
    for _ in 0..2_000 {
        app.update();
        if app.world().resource::<Runner>().status() == RunnerStatus::Done {
            break;
        }
    }
    let runner = app.world().resource::<Runner>();
    assert_eq!(runner.status(), RunnerStatus::Done, "the run finished");
    let report = runner.report();
    assert_eq!(report.tests.len(), 1, "{report:?}");
    assert_eq!(report.tests[0].name, "arena_smoke");
    assert_eq!(report.exit_code(), EXIT_PASSED, "{report:?}");
}

#[test]
fn the_failing_smoke_test_fails_the_run() {
    let mut app = app_with(arena::smoke::install_failing);
    app.update();
    app.world_mut().resource_mut::<Runner>().start(
        Selection {
            name_globs: Vec::new(),
            tags: vec!["smoke_fail".into()],
        },
        RunOptions::default(),
    );
    for _ in 0..2_000 {
        app.update();
        if app.world().resource::<Runner>().status() == RunnerStatus::Done {
            break;
        }
    }
    let report = app.world().resource::<Runner>().report().clone();
    assert_eq!(report.tests.len(), 1, "{report:?}");
    assert_ne!(report.exit_code(), EXIT_PASSED, "{report:?}");
}
