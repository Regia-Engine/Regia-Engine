// SPDX-License-Identifier: MPL-2.0
//! Spec 11 § 2 `pause_on_focus_loss` (lane post05-ui-input, ADR-7070): with the setting on, the primary
//! window losing focus pauses gameplay `Time` and regaining it resumes; with it off (the default) focus
//! changes leave `Time` running; a pause the game set itself is never lifted by a focus gain. Headless:
//! `PlatformPlugin` with no event loop, fixed 16 ms steps.

use std::time::Duration;

use regia_app::prelude::*;
use regia_platform::{PauseOnFocusLoss, PlatformPlugin, PrimaryWindow, Window, WindowFocused};

fn focus_app(setting: bool) -> (App, Entity) {
    let mut app = App::new();
    app.add_plugins(PlatformPlugin)
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)))
        .insert_resource(PauseOnFocusLoss(setting));
    let window = app.world_mut().spawn((Window::default(), PrimaryWindow)).id();
    app.update();
    (app, window)
}

fn focus(app: &mut App, window: Entity, focused: bool) {
    app.world_mut().write_message(WindowFocused { window, focused });
    app.update();
}

fn advancing(app: &mut App) -> bool {
    let before = app.world().resource::<Time>().elapsed();
    app.update();
    app.world().resource::<Time>().elapsed() > before
}

#[test]
fn losing_focus_pauses_time_only_with_the_setting_on() {
    let (mut app, window) = focus_app(true);
    assert!(advancing(&mut app));
    focus(&mut app, window, false);
    assert!(app.world().resource::<Time>().paused, "focus loss pauses");
    assert!(!advancing(&mut app), "gameplay time stops");
    focus(&mut app, window, true);
    assert!(!app.world().resource::<Time>().paused, "focus gain resumes");
    assert!(advancing(&mut app));

    let (mut app, window) = focus_app(false);
    focus(&mut app, window, false);
    assert!(!app.world().resource::<Time>().paused, "setting off: no pause");
    assert!(advancing(&mut app));
}

#[test]
fn a_pause_the_game_set_is_not_lifted_by_focus_gain() {
    let (mut app, window) = focus_app(true);
    app.world_mut().resource_mut::<Time>().paused = true;
    focus(&mut app, window, false);
    focus(&mut app, window, true);
    assert!(
        app.world().resource::<Time>().paused,
        "the game's own pause survives a focus round trip"
    );
}
