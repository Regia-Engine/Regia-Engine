// SPDX-License-Identifier: MPL-2.0
//! Pointer claims (ADR-7976, `docs/specs/18-runtime-ui.md` § 4): a mouse press that lands on a
//! regia_ui widget belongs to the UI from press to release, so no gameplay action bound to that
//! button sees its press, hold or release. A press that starts on the world stays the game's, even
//! when the pointer then moves over a widget. Driven through the real `UiPlugin` and `InputPlugin`
//! with platform messages, as a window delivers them. Headless: no window, no GPU, 60 Hz frames on
//! the default 60 Hz fixed step.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use regia_app::TimeUpdateStrategy;
use regia_app::prelude::*;
use regia_input::prelude::*;
use regia_input::{ActionPhase, Trigger};
use regia_platform::{ButtonState, MouseButton};
use regia_ui::LayerInputMode;
use regia_ui::prelude::*;

/// Every trigger shape a gameplay binding on a mouse button can have.
const LEFT_ACTIONS: [&str; 6] = [
    "Fire",
    "FirePressed",
    "FireReleased",
    "FireHold",
    "FireTap",
    "FirePulse",
];

fn app() -> App {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    app.add_plugins(UiPlugin::code_only());
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::ImeEvent>();
    let left = InputSource::mouse_button(MouseButton::Left);
    app.add_plugins(InputPlugin::new(
        InputConfig::new()
            .action("Fire", ActionKind::Button)
            .action("FirePressed", ActionKind::Button)
            .action("FireReleased", ActionKind::Button)
            .action("FireHold", ActionKind::Button)
            .action("FireTap", ActionKind::Button)
            .action("FirePulse", ActionKind::Button)
            .action("Aim", ActionKind::Button)
            .binding(Binding::new("Fire", left.clone()))
            .binding(Binding::new("FirePressed", left.clone()).with_trigger(Trigger::Pressed))
            .binding(Binding::new("FireReleased", left.clone()).with_trigger(Trigger::Released))
            .binding(Binding::new("FireHold", left.clone()).with_trigger(Trigger::Hold(0.03)))
            .binding(Binding::new("FireTap", left.clone()).with_trigger(Trigger::Tap(0.5)))
            .binding(Binding::new("FirePulse", left).with_trigger(Trigger::Pulse(0.02)))
            .binding(Binding::new("Aim", InputSource::mouse_button(MouseButton::Right))),
    ));
    app.update();
    app
}

/// A clickable widget at window px `[0, 0, 100, 100]` (laid out by hand: no layout pass needed).
fn widget(app: &mut App) -> Entity {
    let rect = [0.0, 0.0, 100.0, 100.0];
    app.world_mut()
        .spawn((
            Interactable::default(),
            ComputedNode {
                rect,
                clip_rect: rect,
                scale: 1.0,
                ..Default::default()
            },
        ))
        .id()
}

fn cursor(app: &mut App, x: f64, y: f64) {
    app.world_mut().write_message(regia_platform::CursorMoved {
        window: Entity::PLACEHOLDER,
        position: (x, y),
        timestamp: Default::default(),
    });
}

fn button(app: &mut App, button: MouseButton, state: ButtonState) {
    app.world_mut().write_message(regia_platform::MouseButtonInput {
        window: Entity::PLACEHOLDER,
        button,
        state,
        timestamp: Default::default(),
    });
}

/// The actions among `names` that gameplay saw this frame: any phase but `Idle`, any press edge,
/// or any active bit.
fn seen(app: &App, names: &[&str]) -> Vec<String> {
    let actions = app.world().resource::<Actions>();
    names
        .iter()
        .filter(|name| {
            actions.phase(name) != ActionPhase::Idle
                || actions.just_pressed_this_frame(name)
                || actions.pressed(name)
                || actions.value(name) != regia_core::math::Vec3::ZERO
        })
        .map(|name| (*name).to_owned())
        .collect()
}

/// The actions among `names` gameplay acted on this frame: any phase but `Idle`, a press edge or
/// the active bit (not the raw value, which an idle chord still carries).
fn fired(app: &App, names: &[&str]) -> Vec<String> {
    let actions = app.world().resource::<Actions>();
    names
        .iter()
        .filter(|name| {
            actions.phase(name) != ActionPhase::Idle
                || actions.just_pressed_this_frame(name)
                || actions.pressed(name)
        })
        .map(|name| (*name).to_owned())
        .collect()
}

fn clicks(app: &mut App, target: Entity) -> usize {
    app.world_mut()
        .resource_mut::<Messages<UiEvent>>()
        .drain()
        .filter(|e| e.target == target && e.kind == UiEventKind::Click)
        .count()
}

#[test]
fn a_click_on_a_widget_never_fires_gameplay() {
    let mut app = app();
    let target = widget(&mut app);
    cursor(&mut app, 50.0, 50.0);
    app.update();
    assert!(seen(&app, &LEFT_ACTIONS).is_empty());

    button(&mut app, MouseButton::Left, ButtonState::Pressed);
    app.update();
    assert_eq!(
        seen(&app, &LEFT_ACTIONS),
        Vec::<String>::new(),
        "a press on a widget fired gameplay on the press frame"
    );
    for frame in 0..6 {
        app.update();
        assert_eq!(
            seen(&app, &LEFT_ACTIONS),
            Vec::<String>::new(),
            "a held press on a widget reached gameplay (held frame {frame})"
        );
    }
    button(&mut app, MouseButton::Left, ButtonState::Released);
    app.update();
    assert_eq!(
        seen(&app, &LEFT_ACTIONS),
        Vec::<String>::new(),
        "the release of a widget press reached gameplay"
    );
    app.update();
    assert_eq!(seen(&app, &LEFT_ACTIONS), Vec::<String>::new());
    assert_eq!(clicks(&mut app, target), 1, "the widget still gets its click");
}

#[test]
fn a_press_on_empty_screen_still_fires() {
    let mut app = app();
    widget(&mut app);
    cursor(&mut app, 300.0, 300.0);
    app.update();
    button(&mut app, MouseButton::Left, ButtonState::Pressed);
    app.update();
    let actions = app.world().resource::<Actions>();
    assert!(actions.just_pressed_this_frame("Fire"), "a world press fires");
    assert!(actions.just_pressed_this_frame("FirePressed"));
    assert!(actions.pressed("Fire"));
    app.update();
    assert!(app.world().resource::<Actions>().pressed("Fire"));
    button(&mut app, MouseButton::Left, ButtonState::Released);
    app.update();
    let actions = app.world().resource::<Actions>();
    assert!(!actions.pressed("Fire"));
    assert!(
        actions.just_pressed_this_frame("FireReleased"),
        "the world release fires"
    );
    assert!(
        actions.just_pressed_this_frame("FireTap"),
        "a short world press is a tap"
    );
}

#[test]
fn a_world_press_dragged_over_a_widget_keeps_firing() {
    let mut app = app();
    widget(&mut app);
    cursor(&mut app, 300.0, 300.0);
    app.update();
    button(&mut app, MouseButton::Left, ButtonState::Pressed);
    app.update();
    assert!(app.world().resource::<Actions>().pressed("Fire"));
    cursor(&mut app, 50.0, 50.0);
    for frame in 0..4 {
        app.update();
        assert!(
            app.world().resource::<Actions>().pressed("Fire"),
            "a world press lost to the widget it moved over (frame {frame})"
        );
    }
    button(&mut app, MouseButton::Left, ButtonState::Released);
    app.update();
    let actions = app.world().resource::<Actions>();
    assert!(!actions.pressed("Fire"));
    assert!(
        actions.just_pressed_this_frame("FireReleased"),
        "the release over the widget is the game's"
    );
}

#[test]
fn a_widget_press_dragged_off_stays_claimed_until_release() {
    let mut app = app();
    widget(&mut app);
    cursor(&mut app, 50.0, 50.0);
    app.update();
    button(&mut app, MouseButton::Left, ButtonState::Pressed);
    app.update();
    cursor(&mut app, 300.0, 300.0);
    for frame in 0..4 {
        app.update();
        assert_eq!(
            seen(&app, &LEFT_ACTIONS),
            Vec::<String>::new(),
            "a widget press dragged off the widget reached gameplay (frame {frame})"
        );
    }
    button(&mut app, MouseButton::Left, ButtonState::Released);
    app.update();
    assert_eq!(
        seen(&app, &LEFT_ACTIONS),
        Vec::<String>::new(),
        "its release off the widget"
    );
    // The latch ended with the release: the next press, off the widget, is the game's.
    button(&mut app, MouseButton::Left, ButtonState::Pressed);
    app.update();
    assert!(app.world().resource::<Actions>().just_pressed_this_frame("Fire"));
}

#[test]
fn a_one_batch_click_on_a_widget_never_fires() {
    let mut app = app();
    let target = widget(&mut app);
    cursor(&mut app, 50.0, 50.0);
    app.update();
    button(&mut app, MouseButton::Left, ButtonState::Pressed);
    button(&mut app, MouseButton::Left, ButtonState::Released);
    for frame in 0..3 {
        app.update();
        assert_eq!(
            seen(&app, &LEFT_ACTIONS),
            Vec::<String>::new(),
            "a press and release in one batch on a widget reached gameplay (frame {frame})"
        );
    }
    assert_eq!(clicks(&mut app, target), 1, "the widget still gets its click");
}

#[test]
fn right_clicks_on_a_widget_never_fire_gameplay() {
    let mut app = app();
    widget(&mut app);
    cursor(&mut app, 50.0, 50.0);
    app.update();
    button(&mut app, MouseButton::Right, ButtonState::Pressed);
    app.update();
    assert_eq!(
        seen(&app, &["Aim"]),
        Vec::<String>::new(),
        "a right press on a widget"
    );
    button(&mut app, MouseButton::Right, ButtonState::Released);
    app.update();
    assert_eq!(seen(&app, &["Aim"]), Vec::<String>::new(), "its release");
    // Off the widget, the right button is the game's.
    cursor(&mut app, 300.0, 300.0);
    button(&mut app, MouseButton::Right, ButtonState::Pressed);
    app.update();
    assert!(app.world().resource::<Actions>().just_pressed_this_frame("Aim"));
}

#[test]
fn a_game_only_layer_leaves_presses_to_gameplay() {
    let mut app = app();
    let target = widget(&mut app);
    regia_ui::layers::push_layer(app.world_mut(), target, LayerInputMode::GameOnly).unwrap();
    cursor(&mut app, 50.0, 50.0);
    app.update();
    button(&mut app, MouseButton::Left, ButtonState::Pressed);
    app.update();
    assert!(
        app.world().resource::<Actions>().just_pressed_this_frame("Fire"),
        "the UI ignores the pointer under a game_only layer, so it claims nothing"
    );
}

/// ADR-8786: with claims on, the ADR-7070 gate (`UiPointerGate`) follows press ownership, not
/// hover: hovering a widget leaves it clear, a widget press sets it until the release.
#[test]
fn the_pointer_gate_follows_a_widget_press_not_hover() {
    let mut app = app();
    widget(&mut app);
    cursor(&mut app, 50.0, 50.0);
    app.update();
    let gate = |app: &App| app.world().resource::<regia_input::UiPointerGate>().ui_consumed;
    assert!(
        !gate(&app),
        "hover alone does not gate gameplay while claims are on"
    );
    button(&mut app, MouseButton::Left, ButtonState::Pressed);
    app.update();
    assert!(gate(&app), "a widget press gates mouse bindings while it lasts");
    cursor(&mut app, 300.0, 300.0);
    app.update();
    assert!(gate(&app), "dragged off the widget, the press is still the UI's");
    button(&mut app, MouseButton::Left, ButtonState::Released);
    app.update();
    assert!(!gate(&app), "the release ends it");
}

/// ADR-8786: a host that turns claims off keeps ADR-7070's hover gate (previous frame's hit).
#[test]
fn with_claims_off_the_hover_gate_still_hides_a_press_over_a_widget() {
    let mut app = app();
    app.world_mut()
        .resource_mut::<regia_input::PointerClaims>()
        .set_enabled(false);
    widget(&mut app);
    cursor(&mut app, 50.0, 50.0);
    app.update();
    assert!(app.world().resource::<regia_input::UiPointerGate>().ui_consumed);
    button(&mut app, MouseButton::Left, ButtonState::Pressed);
    app.update();
    assert_eq!(seen(&app, &LEFT_ACTIONS), Vec::<String>::new());
    button(&mut app, MouseButton::Left, ButtonState::Released);
    app.update();
    // Off the widget the gate clears one frame later, then the press is the game's.
    cursor(&mut app, 300.0, 300.0);
    app.update();
    button(&mut app, MouseButton::Left, ButtonState::Pressed);
    app.update();
    assert!(app.world().resource::<Actions>().just_pressed_this_frame("Fire"));
}

/// A chord on the mouse button (`Melee` = left while `Shift` is held) and a chord whose partner
/// is the mouse button (`Combo` = `E` while `Fire` is held): a widget click reaches neither.
fn chord_app() -> App {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    app.add_plugins(UiPlugin::code_only());
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::ImeEvent>();
    let left = InputSource::mouse_button(MouseButton::Left);
    app.add_plugins(InputPlugin::new(
        InputConfig::new()
            .action("Shift", ActionKind::Button)
            .action("Fire", ActionKind::Button)
            .action("Melee", ActionKind::Button)
            .action("Combo", ActionKind::Button)
            .binding(Binding::new("Shift", InputSource::key("ShiftLeft")))
            .binding(Binding::new("Fire", left.clone()))
            .binding(Binding::new("Melee", left).with_trigger(Trigger::Chord("Shift".into())))
            .binding(
                Binding::new("Combo", InputSource::key("KeyE")).with_trigger(Trigger::Chord("Fire".into())),
            ),
    ));
    app.update();
    app
}

fn key(app: &mut App, code: &str, state: ButtonState) {
    app.world_mut().write_message(regia_platform::KeyboardInput {
        window: Entity::PLACEHOLDER,
        physical_key: code.into(),
        logical_key: code.into(),
        state,
        repeat: false,
        timestamp: Default::default(),
    });
}

#[test]
fn chords_on_a_claimed_button_never_fire() {
    let mut app = chord_app();
    widget(&mut app);
    cursor(&mut app, 50.0, 50.0);
    key(&mut app, "ShiftLeft", ButtonState::Pressed);
    key(&mut app, "KeyE", ButtonState::Pressed);
    app.update();
    app.update();
    assert!(
        app.world().resource::<Actions>().pressed("Shift"),
        "the keys are the game's"
    );
    // `E` alone writes Combo's raw value but leaves the chord idle: judge Combo by its phase.
    assert_eq!(
        fired(&app, &["Combo"]),
        Vec::<String>::new(),
        "E alone is no combo"
    );
    button(&mut app, MouseButton::Left, ButtonState::Pressed);
    for frame in 0..4 {
        app.update();
        assert_eq!(
            seen(&app, &["Fire", "Melee"]),
            Vec::<String>::new(),
            "a chord on the mouse saw a widget press (frame {frame})"
        );
        assert_eq!(
            fired(&app, &["Combo"]),
            Vec::<String>::new(),
            "a chord partnered with the mouse saw a widget press (frame {frame})"
        );
    }
    button(&mut app, MouseButton::Left, ButtonState::Released);
    app.update();
    assert_eq!(seen(&app, &["Fire", "Melee"]), Vec::<String>::new());
    assert_eq!(fired(&app, &["Combo"]), Vec::<String>::new());
    // Control: the same chord with the press on empty screen fires.
    cursor(&mut app, 300.0, 300.0);
    button(&mut app, MouseButton::Left, ButtonState::Pressed);
    app.update();
    app.update();
    let actions = app.world().resource::<Actions>();
    assert!(actions.pressed("Melee"), "a world press with Shift is a melee");
    assert!(actions.pressed("Combo"), "E with a world Fire is a combo");
}
