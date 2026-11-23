// SPDX-License-Identifier: MPL-2.0
//! Tooltip and hint behavior end to end (lane ui-d13): focus-triggered tooltips under gamepad
//! navigation, dismiss on B and A, the navigation-text variant with device glyphs, and the pointer
//! path. Headless; every wait is a frame count at a fixed 16 ms step, never a clock.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use regia_app::TimeUpdateStrategy;
use regia_app::prelude::*;
use regia_platform::{ButtonState, GamepadButton, GamepadButtonChanged};
use regia_ui::focus::{DefaultFocus, Focusable, UiInputMode};
use regia_ui::prelude::*;
use regia_ui::{HintDevice, TooltipNavText, TooltipState};

fn app() -> App {
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
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
    app
}

fn tip_node(app: &mut App, rect: [f32; 4], text: &str) -> Entity {
    app.world_mut()
        .spawn((
            Focusable::default(),
            Tooltip::new(text),
            ComputedNode {
                rect,
                clip_rect: rect,
                scale: 1.0,
                ..Default::default()
            },
        ))
        .id()
}

fn press(app: &mut App, button: GamepadButton) {
    for state in [ButtonState::Pressed, ButtonState::Released] {
        app.world_mut().write_message(GamepadButtonChanged {
            device: 1,
            button,
            state,
            value: 1.0,
        });
        app.update();
    }
}

fn frames(app: &mut App, n: u32) {
    for _ in 0..n {
        app.update();
    }
}

fn visible(app: &App, e: Entity) -> bool {
    app.world().get::<Tooltip>(e).unwrap().visible
}

#[test]
fn focus_opens_the_tooltip_after_the_focus_delay_and_b_dismisses_it() {
    let mut app = app();
    let a = tip_node(&mut app, [100.0, 100.0, 200.0, 50.0], "Buy the thing");
    app.world_mut().entity_mut(a).insert(DefaultFocus);
    press(&mut app, GamepadButton::DPadDown);
    assert_eq!(*app.world().resource::<UiInputMode>(), UiInputMode::Navigation);
    assert!(!visible(&app, a), "not open the moment focus lands");

    // 0.5 s node delay + 0.3 s focus extra = 0.8 s = 50 frames of 16 ms; 40 is short, 70 is past.
    frames(&mut app, 40);
    assert!(!visible(&app, a), "still waiting before the focus delay");
    frames(&mut app, 30);
    assert!(visible(&app, a));
    assert_eq!(
        app.world().resource::<TooltipState>().text(),
        Some("Buy the thing")
    );

    // B dismisses and the tooltip stays closed while focus stays put.
    press(&mut app, GamepadButton::East);
    assert!(!visible(&app, a));
    frames(&mut app, 120);
    assert!(!visible(&app, a));
    assert_eq!(app.world().resource::<TooltipState>().text(), None);
}

#[test]
fn a_press_on_the_focused_node_dismisses_the_tooltip() {
    let mut app = app();
    let a = tip_node(&mut app, [100.0, 100.0, 200.0, 50.0], "Hint");
    app.world_mut().entity_mut(a).insert(DefaultFocus);
    press(&mut app, GamepadButton::DPadDown);
    frames(&mut app, 80);
    assert!(visible(&app, a));
    press(&mut app, GamepadButton::South);
    assert!(!visible(&app, a));
    frames(&mut app, 120);
    assert!(!visible(&app, a));
}

#[test]
fn navigation_text_uses_the_glyph_of_the_last_device() {
    let mut app = app();
    let a = tip_node(&mut app, [100.0, 100.0, 200.0, 50.0], "Buy");
    app.world_mut()
        .entity_mut(a)
        .insert((DefaultFocus, TooltipNavText("Press {activate} to buy".into())));
    app.insert_resource(HintDevice::PlayStation);
    press(&mut app, GamepadButton::DPadDown);
    frames(&mut app, 80);
    assert_eq!(
        app.world().resource::<TooltipState>().text(),
        Some("Press Cross to buy")
    );
}

#[test]
fn moving_focus_between_tooltips_is_instant_while_warm() {
    let mut app = app();
    let a = tip_node(&mut app, [100.0, 100.0, 200.0, 50.0], "First");
    let b = tip_node(&mut app, [100.0, 180.0, 200.0, 50.0], "Second");
    app.world_mut().entity_mut(a).insert(DefaultFocus);
    press(&mut app, GamepadButton::DPadDown);
    frames(&mut app, 80);
    assert!(visible(&app, a));
    press(&mut app, GamepadButton::DPadDown);
    frames(&mut app, 1);
    assert!(!visible(&app, a));
    assert!(visible(&app, b), "the warm window skips the delay");
}

#[test]
fn a_node_without_focus_never_shows_in_navigation_mode() {
    let mut app = app();
    let a = tip_node(&mut app, [100.0, 100.0, 200.0, 50.0], "A");
    let b = tip_node(&mut app, [100.0, 180.0, 200.0, 50.0], "B");
    app.world_mut().entity_mut(a).insert(DefaultFocus);
    press(&mut app, GamepadButton::DPadDown);
    frames(&mut app, 80);
    assert!(visible(&app, a));
    assert!(!visible(&app, b));
}

/// Review rev-kimi-20 finding 1: showing a tooltip must not write the expanded or navigation hint
/// text back into the authored `Tooltip.text`. The popup shows the expanded text; the component
/// keeps "Buy" (and its placeholders) for the next show.
#[test]
fn showing_a_tooltip_keeps_its_authored_text() {
    let mut app = app();
    let a = tip_node(&mut app, [100.0, 100.0, 200.0, 50.0], "Buy");
    app.world_mut()
        .entity_mut(a)
        .insert((DefaultFocus, TooltipNavText("Press {activate} to buy".into())));
    app.insert_resource(HintDevice::PlayStation);
    press(&mut app, GamepadButton::DPadDown);
    frames(&mut app, 80);
    assert!(visible(&app, a));
    assert_eq!(
        app.world().resource::<TooltipState>().text(),
        Some("Press Cross to buy")
    );
    assert_eq!(app.world().get::<Tooltip>(a).unwrap().text, "Buy");
}

/// The popup label follows the hint text (the expanded navigation text) while the authored text
/// stays put, and a device switch while open re-expands the placeholders.
#[test]
fn the_popup_label_shows_the_expanded_hint_and_follows_a_device_switch() {
    let mut app = app();
    let a = tip_node(&mut app, [100.0, 100.0, 200.0, 50.0], "Buy {activate}");
    app.world_mut().entity_mut(a).insert(DefaultFocus);
    app.insert_resource(HintDevice::PlayStation);
    press(&mut app, GamepadButton::DPadDown);
    frames(&mut app, 80);
    assert!(visible(&app, a));
    let popup_text = |app: &mut App| -> Vec<String> {
        let mut q = app.world_mut().query_filtered::<&Text, With<UiNode>>();
        q.iter(app.world())
            .filter_map(|t| match t.spans.first().map(|s| &s.content) {
                Some(regia_ui::text::TextContent::Lit(s)) => Some(s.clone()),
                _ => None,
            })
            .collect()
    };
    assert_eq!(popup_text(&mut app), vec!["Buy Cross".to_owned()]);
    app.insert_resource(HintDevice::Xbox);
    frames(&mut app, 2);
    assert_eq!(
        app.world().get::<Tooltip>(a).unwrap().text,
        "Buy {activate}",
        "the authored placeholder survives a show"
    );
    assert_eq!(
        popup_text(&mut app),
        vec!["Buy A".to_owned()],
        "the label follows the device switch"
    );
}
