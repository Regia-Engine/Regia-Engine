// SPDX-License-Identifier: MPL-2.0
//! Motion tokens (lane D8, ADR-371; `docs/specs/ui-default-design.md` § 2.6): a state transition
//! follows the widget's `transition` easing, and reduced motion makes it jump. No GPU, no files,
//! fixed 50 ms steps, no wall-clock assertions.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use regia_app::TimeUpdateStrategy;
use regia_app::prelude::*;
use regia_ui::prelude::*;
use regia_ui::{ActiveTheme, Hovered, Themed};

fn ui_app() -> App {
    let mut app = App::new();
    app.add_plugins(UiPlugin::code_only());
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::CursorMoved>()
        .add_message::<regia_platform::MouseButtonInput>()
        .add_message::<regia_platform::TouchInput>()
        .add_message::<regia_platform::MouseWheel>()
        .add_message::<regia_platform::KeyboardInput>()
        .add_message::<regia_platform::ImeEvent>();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(50)));
    app
}

fn themed(app: &App, e: Entity) -> Themed {
    *app.world().get::<Themed>(e).unwrap()
}

fn settle(app: &mut App) {
    for _ in 0..8 {
        app.update();
    }
}

fn button(app: &mut App) -> Entity {
    let e = app
        .world_mut()
        .spawn((UiNode, Classes(vec!["button".to_owned()])))
        .id();
    settle(app);
    e
}

#[test]
fn hover_transition_follows_the_standard_ease() {
    let mut app = ui_app();
    let e = button(&mut app);
    app.world_mut().entity_mut(e).insert(Hovered);
    app.update();
    let t = themed(&app, e);
    assert!(
        t.anim_total > 0.0 && t.anim_remaining > 0.0,
        "the hover fade is running"
    );
    assert_eq!(
        t.anim_ease,
        Some([0.2, 0.0, 0.0, 1.0]),
        "the button's transition names ease.standard"
    );
    let from = t.anim_from[0].0;
    let to = t.anim_to[0].expect("button has a fill").0;
    let ch = (0..3)
        .max_by(|&a, &b| (to[a] - from[a]).abs().total_cmp(&(to[b] - from[b]).abs()))
        .unwrap();
    assert!((to[ch] - from[ch]).abs() > 1e-4, "hover changes the fill");
    let shown = t.background.expect("a shown fill").0;
    let progress = (shown[ch] - from[ch]) / (to[ch] - from[ch]);
    let linear = 1.0 - t.anim_remaining / t.anim_total;
    assert!(
        progress > linear + 0.05,
        "ease.standard leads linear: progress {progress}, linear {linear}"
    );
    settle(&mut app);
    assert_eq!(
        themed(&app, e).background,
        Some(regia_ui::UiColor(to)),
        "ends on the target"
    );
}

#[test]
fn reduced_motion_jumps_to_the_hover_fill() {
    let mut app = ui_app();
    let e = button(&mut app);
    app.world_mut().insert_resource(ActiveTheme {
        reduced_motion: true,
        ..ActiveTheme::default()
    });
    settle(&mut app);
    app.world_mut().entity_mut(e).insert(Hovered);
    app.update();
    let t = themed(&app, e);
    assert_eq!(t.anim_remaining, 0.0, "no animation is running");
    assert_eq!(
        t.background, t.anim_to[0],
        "the fill is already at its hover value"
    );
}

#[test]
fn a_swap_still_cross_fades_without_reduced_motion() {
    let mut app = ui_app();
    let e = button(&mut app);
    app.world_mut().insert_resource(ActiveTheme::named("regia-light"));
    app.update();
    assert!(themed(&app, e).anim_remaining > 0.0, "the swap cross-fades");
}
