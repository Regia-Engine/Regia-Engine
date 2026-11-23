// SPDX-License-Identifier: MPL-2.0
//! Input timestamps, the per-frame `InputTiming` collection, the close policy and `request_quit`,
//! driven through a headless `App` (no window, no event loop).

use std::time::Duration;

use regia_app::prelude::*;
use regia_platform::{
    BackPressed, ButtonState, ClosePolicy, CursorMoved, InputTimestamp, InputTiming, InputTimingPlugin,
    KeyboardInput, MouseMotion, PlatformPlugin, PrimaryWindow, QuitRequest, Window, WindowCloseRequested,
    request_quit,
};

fn stamp(micros: u64) -> InputTimestamp {
    InputTimestamp(Duration::from_micros(micros))
}

fn key(window: Entity, micros: u64) -> KeyboardInput {
    KeyboardInput {
        window,
        physical_key: "KeyA".into(),
        logical_key: "a".into(),
        state: ButtonState::Pressed,
        repeat: false,
        timestamp: stamp(micros),
    }
}

fn timing_app() -> App {
    let mut app = App::new();
    app.add_plugins(InputTimingPlugin);
    app
}

#[test]
fn timestamp_conversions_and_clamping() {
    assert_eq!(InputTimestamp::ZERO.since_epoch(), Duration::ZERO);
    assert_eq!(InputTimestamp::ZERO.as_micros(), 0);
    assert_eq!(InputTimestamp::default(), InputTimestamp::ZERO);
    assert_eq!(stamp(1_500_000).as_micros(), 1_500_000);
    assert_eq!(stamp(2_000_000).since_epoch(), Duration::from_secs(2));
    // A duration beyond u64 microseconds saturates instead of wrapping.
    assert_eq!(InputTimestamp(Duration::MAX).as_micros(), u64::MAX);
    assert!(stamp(1) < stamp(2));

    // The epoch is fixed by the first call; an instant that long ago clamps to zero, and later
    // instants never go backward.
    let epoch = regia_platform::input_epoch();
    assert_eq!(regia_platform::input_epoch(), epoch);
    assert_eq!(InputTimestamp::at(epoch), InputTimestamp::ZERO);
    let a = InputTimestamp::now();
    let b = InputTimestamp::now();
    assert!(b >= a);
    if let Some(before) = epoch.checked_sub(Duration::from_secs(1)) {
        assert_eq!(InputTimestamp::at(before), InputTimestamp::ZERO);
    }
}

#[test]
fn max_latency_is_oldest_event_to_frame_start() {
    let mut timing = InputTiming::default();
    assert_eq!(timing.max_latency(), None);
    timing.frame_start = stamp(10_000);
    timing.events = vec![stamp(4_000), stamp(9_000)];
    assert_eq!(timing.max_latency(), Some(Duration::from_micros(6_000)));
    // An event stamped after the frame start (receipt raced the frame) reports zero, not a panic.
    timing.events = vec![stamp(12_000)];
    assert_eq!(timing.max_latency(), Some(Duration::ZERO));
}

#[test]
fn timing_collects_sorted_nonzero_stamps_from_every_input_kind() {
    let mut app = timing_app();
    let window = app.world_mut().spawn_empty().id();
    app.world_mut().write_message(key(window, 300));
    app.world_mut().write_message(CursorMoved {
        window,
        position: (1.0, 2.0),
        timestamp: stamp(100),
    });
    app.world_mut().write_message(MouseMotion {
        delta: (1.0, 1.0),
        timestamp: stamp(200),
    });
    // The zero stamp marks a synthetic event and is not recorded.
    app.world_mut().write_message(key(window, 0));
    app.update();
    let timing = app.world().resource::<InputTiming>();
    assert_eq!(timing.events, [stamp(100), stamp(200), stamp(300)]);
    assert_eq!(timing.latest, Some(stamp(300)));
}

#[test]
fn timing_forgets_events_each_frame_but_keeps_the_newest_stamp() {
    let mut app = timing_app();
    let window = app.world_mut().spawn_empty().id();
    app.world_mut().write_message(key(window, 500));
    app.update();
    assert_eq!(app.world().resource::<InputTiming>().events, [stamp(500)]);

    app.update();
    let timing = app.world().resource::<InputTiming>();
    assert!(timing.events.is_empty(), "no input this frame");
    assert_eq!(timing.latest, Some(stamp(500)), "latest spans frames");
    assert_eq!(timing.max_latency(), None);

    // An older stamp arriving later does not move `latest` backward.
    app.world_mut().write_message(key(window, 400));
    app.update();
    let timing = app.world().resource::<InputTiming>();
    assert_eq!(timing.events, [stamp(400)]);
    assert_eq!(timing.latest, Some(stamp(500)));
}

#[test]
fn timing_without_any_input_leaves_latest_empty() {
    let mut app = timing_app();
    app.update();
    app.update();
    let timing = app.world().resource::<InputTiming>();
    assert!(timing.events.is_empty());
    assert_eq!(timing.latest, None);
}

#[test]
fn platform_plugin_includes_input_timing() {
    // `PlatformPlugin` brings the timing resource and systems along.
    let mut app = App::new();
    app.add_plugins(PlatformPlugin);
    let window = app.world_mut().spawn_empty().id();
    app.world_mut().write_message(key(window, 77));
    app.update();
    assert_eq!(app.world().resource::<InputTiming>().latest, Some(stamp(77)));
}

#[test]
fn close_policy_defaults_exit_and_back_closes() {
    let policy = ClosePolicy::default();
    assert!(policy.exit_on_primary_close);
    assert!(policy.back_closes_primary);
}

fn platform_app_with_window() -> (App, Entity) {
    let mut app = App::new();
    app.add_plugins(PlatformPlugin);
    let window = app.world_mut().spawn((Window::default(), PrimaryWindow)).id();
    (app, window)
}

#[test]
fn request_quit_headless_exits_when_the_policy_allows() {
    let mut app = App::new();
    app.add_plugins(PlatformPlugin);
    let outcome = request_quit(app.world_mut());
    assert_eq!(
        outcome,
        QuitRequest {
            window: None,
            will_exit: true
        }
    );
    assert_eq!(app.should_exit(), Some(AppExit::Success));
}

#[test]
fn request_quit_headless_stays_running_when_the_game_decides() {
    let mut app = App::new();
    app.add_plugins(PlatformPlugin);
    app.world_mut()
        .resource_mut::<ClosePolicy>()
        .exit_on_primary_close = false;
    let outcome = request_quit(app.world_mut());
    assert_eq!(
        outcome,
        QuitRequest {
            window: None,
            will_exit: false
        }
    );
    assert_eq!(app.should_exit(), None);
}

#[test]
fn request_quit_with_a_window_sends_a_close_request_and_exits_on_the_frame() {
    let (mut app, window) = platform_app_with_window();
    let outcome = request_quit(app.world_mut());
    assert_eq!(outcome.window, Some(window));
    assert!(outcome.will_exit);
    assert_eq!(
        app.should_exit(),
        None,
        "the exit comes from the frame, not the call"
    );
    app.update();
    assert_eq!(app.should_exit(), Some(AppExit::Success));
}

#[test]
fn request_quit_with_a_window_and_exit_policy_off_only_reports_the_close() {
    let (mut app, window) = platform_app_with_window();
    app.world_mut()
        .resource_mut::<ClosePolicy>()
        .exit_on_primary_close = false;
    let outcome = request_quit(app.world_mut());
    assert_eq!(
        outcome,
        QuitRequest {
            window: Some(window),
            will_exit: false
        }
    );
    app.update();
    assert_eq!(app.should_exit(), None);
}

#[test]
fn request_quit_without_a_policy_resource_uses_the_default() {
    let mut world = World::new();
    let outcome = request_quit(&mut world);
    assert_eq!(
        outcome,
        QuitRequest {
            window: None,
            will_exit: true
        }
    );
}

#[test]
fn request_quit_with_a_non_primary_window_asks_it_and_does_not_exit() {
    // No PrimaryWindow, but a Window exists: the close request goes to that window and the app
    // does not exit by itself, so the game's close handler decides.
    let mut app = App::new();
    app.add_plugins(PlatformPlugin);
    let window = app.world_mut().spawn(Window::default()).id();
    let outcome = request_quit(app.world_mut());
    assert_eq!(
        outcome,
        QuitRequest {
            window: Some(window),
            will_exit: false
        }
    );
    let requests = app.world().resource::<Messages<WindowCloseRequested>>();
    let seen: Vec<Entity> = requests
        .iter_current_update_messages()
        .map(|r| r.window)
        .collect();
    assert_eq!(seen, [window], "one close request, for the only window");
    app.update();
    assert_eq!(
        app.should_exit(),
        None,
        "only the primary window's close exits by itself"
    );
}

#[test]
fn back_button_closes_the_primary_window_by_default() {
    let (mut app, _window) = platform_app_with_window();
    app.world_mut().write_message(BackPressed);
    app.update();
    assert_eq!(app.should_exit(), Some(AppExit::Success));
}

#[test]
fn back_button_is_ignored_when_the_policy_turns_it_off() {
    let (mut app, _window) = platform_app_with_window();
    app.world_mut().resource_mut::<ClosePolicy>().back_closes_primary = false;
    app.world_mut().write_message(BackPressed);
    app.update();
    assert_eq!(app.should_exit(), None);
}

/// A screen turns the policy off while it is open, Back closes it, and the policy comes back the
/// next frame (the first-person template's touch Controls screen). A Back written after `First`
/// (an agent's `play.inject_input` runs in `PreUpdate`) is still buffered in that next frame: the
/// close handler must not act on it once the policy is back on.
#[test]
fn a_back_pressed_while_the_policy_was_off_never_closes_later() {
    #[derive(Resource, Default)]
    struct Frame(u32);
    let (mut app, _window) = platform_app_with_window();
    app.init_resource::<Frame>();
    app.add_systems(
        PreUpdate,
        |mut frame: ResMut<Frame>, mut back: MessageWriter<BackPressed>| {
            frame.0 += 1;
            if frame.0 == 2 {
                back.write(BackPressed);
            }
        },
    );
    app.add_systems(Update, |frame: Res<Frame>, mut policy: ResMut<ClosePolicy>| {
        // Off on frames 1 and 2 (the screen is open), back on from frame 3.
        policy.back_closes_primary = frame.0 >= 3;
    });
    for frame in 1..=4 {
        app.update();
        assert_eq!(
            app.should_exit(),
            None,
            "frame {frame}: the Back of frame 2 met a policy that was off; it must not quit later"
        );
    }
}

#[test]
fn back_button_asks_but_does_not_exit_when_exit_on_close_is_off() {
    let (mut app, window) = platform_app_with_window();
    app.world_mut()
        .resource_mut::<ClosePolicy>()
        .exit_on_primary_close = false;
    app.world_mut().write_message(BackPressed);
    app.update();
    assert_eq!(app.should_exit(), None);
    // The close request itself is still delivered for the game to handle.
    let requests = app.world().resource::<Messages<WindowCloseRequested>>();
    let seen: Vec<Entity> = requests
        .iter_current_update_messages()
        .map(|r| r.window)
        .collect();
    assert_eq!(seen, [window]);
}

#[test]
fn closing_a_secondary_window_does_not_exit() {
    let (mut app, _primary) = platform_app_with_window();
    let secondary = app.world_mut().spawn(Window::default()).id();
    app.world_mut()
        .write_message(WindowCloseRequested { window: secondary });
    app.update();
    assert_eq!(app.should_exit(), None);
}

#[test]
fn back_button_without_a_primary_window_does_nothing() {
    let mut app = App::new();
    app.add_plugins(PlatformPlugin);
    app.world_mut().write_message(BackPressed);
    app.update();
    assert_eq!(app.should_exit(), None);
}
