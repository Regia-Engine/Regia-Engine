// SPDX-License-Identifier: MPL-2.0
//! Run-mode planning through the public `plan_quick_frame` system: zero budgets, mode changes,
//! each wake-up message kind, batching, and frame-pacing restore. Complements the unit tests in
//! `run_mode.rs` (basic loop, loop-n-times and wait).

use regia_app::prelude::*;
use regia_platform::{
    ButtonState, CursorMoved, InputTimestamp, KeyboardInput, MouseButton, MouseButtonInput, WindowResized,
    WindowScaleFactorChanged,
};
use regia_quick::run_mode::{FROZEN_IDLE_FPS, WAIT_IDLE_FPS, plan_quick_frame};
use regia_quick::{DrawFrozen, QuickLoop, RunMode};

fn planner(mode: RunMode, pacing: Option<Option<f32>>) -> App {
    let mut app = App::new();
    app.add_message::<KeyboardInput>()
        .add_message::<MouseButtonInput>()
        .add_message::<CursorMoved>()
        .add_message::<regia_platform::MouseWheel>()
        .add_message::<regia_platform::TouchInput>()
        .add_message::<WindowResized>()
        .add_message::<WindowScaleFactorChanged>()
        .insert_resource(mode)
        .init_resource::<QuickLoop>()
        .init_resource::<DrawFrozen>()
        .add_systems(Update, plan_quick_frame);
    if let Some(target) = pacing {
        app.insert_resource(FramePacing::new(target));
    }
    app
}

fn ran(app: &App) -> bool {
    app.world().resource::<QuickLoop>().run_this_frame
}

fn runs_over(app: &mut App, frames: u32) -> u32 {
    let mut runs = 0;
    for _ in 0..frames {
        app.update();
        runs += u32::from(ran(app));
    }
    runs
}

#[test]
fn run_mode_constructors_and_default() {
    assert_eq!(RunMode::default(), RunMode::Loop);
    assert_eq!(RunMode::loop_once(), RunMode::LoopNTimes(1));
    assert_eq!(RunMode::loop_ntimes(7), RunMode::LoopNTimes(7));
    assert_eq!(QuickLoop::default().runs, 0);
    assert!(!QuickLoop::default().frozen);
}

#[test]
fn a_zero_budget_never_runs_and_is_frozen_from_the_first_frame() {
    let mut app = planner(RunMode::loop_ntimes(0), Some(None));
    app.update();
    assert!(!ran(&app));
    assert!(app.world().resource::<QuickLoop>().frozen);
    assert!(app.world().resource::<DrawFrozen>().0);
    assert_eq!(app.world().resource::<QuickLoop>().runs, 0);
    assert_eq!(runs_over(&mut app, 5), 0);
    assert_eq!(
        app.world().resource::<FramePacing>().target_fps,
        Some(FROZEN_IDLE_FPS)
    );
}

#[test]
fn a_one_frame_budget_runs_exactly_once() {
    let mut app = planner(RunMode::loop_once(), None);
    assert_eq!(runs_over(&mut app, 6), 1);
    assert_eq!(app.world().resource::<QuickLoop>().runs, 1);
}

#[test]
fn switching_modes_restarts_the_budget_for_the_new_mode() {
    let mut app = planner(RunMode::loop_ntimes(2), None);
    assert_eq!(runs_over(&mut app, 6), 2);
    // A different budget is a different mode: it runs its own count.
    app.insert_resource(RunMode::loop_ntimes(3));
    assert_eq!(runs_over(&mut app, 8), 3);
    assert!(app.world().resource::<QuickLoop>().frozen);
    // Back to Loop: runs every frame and the drawing is live again.
    app.insert_resource(RunMode::Loop);
    assert_eq!(runs_over(&mut app, 4), 4);
    assert!(!app.world().resource::<QuickLoop>().frozen);
    assert!(!app.world().resource::<DrawFrozen>().0);
}

#[test]
fn restarting_the_same_mode_through_the_loop_state_replays_the_budget() {
    let mut app = planner(RunMode::loop_ntimes(2), None);
    assert_eq!(runs_over(&mut app, 5), 2);
    // `Quick::set_run_mode` resets the counters like this (see context_api.rs for the real call).
    *app.world_mut().resource_mut::<QuickLoop>() = QuickLoop::default();
    assert_eq!(
        runs_over(&mut app, 5),
        2,
        "the same budget runs again after a restart"
    );
}

const WAKERS: [&str; 5] = ["key", "mouse button", "cursor", "resize", "scale factor"];

fn send(name: &str, world: &mut World) {
    let window = Entity::PLACEHOLDER;
    let timestamp = InputTimestamp::ZERO;
    match name {
        "key" => {
            world.write_message(KeyboardInput {
                window,
                physical_key: "Space".into(),
                logical_key: "Space".into(),
                state: ButtonState::Pressed,
                repeat: false,
                timestamp,
            });
        }
        "mouse button" => {
            world.write_message(MouseButtonInput {
                window,
                button: MouseButton::Left,
                state: ButtonState::Pressed,
                timestamp,
            });
        }
        "cursor" => {
            world.write_message(CursorMoved {
                window,
                position: (1.0, 2.0),
                timestamp,
            });
        }
        "resize" => {
            world.write_message(WindowResized {
                window,
                width: 10,
                height: 20,
            });
        }
        _ => {
            world.write_message(WindowScaleFactorChanged {
                window,
                scale_factor: 2.0,
            });
        }
    }
}

#[test]
fn every_kind_of_wake_up_message_runs_a_wait_app() {
    for name in WAKERS {
        let mut app = planner(RunMode::Wait, None);
        app.update();
        app.update();
        assert!(!ran(&app), "{name}: idle before the message");
        send(name, app.world_mut());
        app.update();
        assert!(ran(&app), "{name}: the message wakes the app");
        app.update();
        assert!(!ran(&app), "{name}: one message wakes one frame");
    }
}

#[test]
fn many_messages_in_one_frame_run_one_frame_and_are_all_consumed() {
    let mut app = planner(RunMode::Wait, None);
    app.update();
    for i in 0..5 {
        app.world_mut().write_message(CursorMoved {
            window: Entity::PLACEHOLDER,
            position: (f64::from(i), 0.0),
            timestamp: InputTimestamp::ZERO,
        });
    }
    app.world_mut().write_message(WindowResized {
        window: Entity::PLACEHOLDER,
        width: 1,
        height: 1,
    });
    app.update();
    assert!(ran(&app));
    assert_eq!(
        app.world().resource::<QuickLoop>().runs,
        2,
        "first frame + one wake"
    );
    // Neither the cursor nor the resize reader leaves a stale message behind.
    assert_eq!(runs_over(&mut app, 4), 0);
}

#[test]
fn wait_idles_at_the_wait_rate_and_restores_the_apps_own_pacing() {
    // Uncapped app: idles at WAIT_IDLE_FPS, back to uncapped on a wake.
    let mut app = planner(RunMode::Wait, Some(None));
    app.update();
    app.update();
    assert_eq!(
        app.world().resource::<FramePacing>().target_fps,
        Some(WAIT_IDLE_FPS)
    );
    app.world_mut().write_message(CursorMoved {
        window: Entity::PLACEHOLDER,
        position: (0.0, 0.0),
        timestamp: InputTimestamp::ZERO,
    });
    app.update();
    assert_eq!(app.world().resource::<FramePacing>().target_fps, None);

    // An app capped below the idle rate keeps its cap while idling and gets it back.
    let mut app = planner(RunMode::Wait, Some(Some(5.0)));
    app.update();
    app.update();
    assert_eq!(app.world().resource::<FramePacing>().target_fps, Some(5.0));
    // An app capped above the idle rate is slowed to it, then restored.
    let mut app = planner(RunMode::Wait, Some(Some(144.0)));
    app.update();
    app.update();
    assert_eq!(
        app.world().resource::<FramePacing>().target_fps,
        Some(WAIT_IDLE_FPS)
    );
    app.insert_resource(RunMode::Loop);
    app.update();
    assert_eq!(app.world().resource::<FramePacing>().target_fps, Some(144.0));
}

#[test]
fn loop_mode_leaves_the_pacing_alone() {
    let mut app = planner(RunMode::Loop, Some(Some(60.0)));
    assert_eq!(runs_over(&mut app, 3), 3);
    assert_eq!(app.world().resource::<FramePacing>().target_fps, Some(60.0));
}
