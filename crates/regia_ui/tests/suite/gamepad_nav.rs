// SPDX-License-Identifier: MPL-2.0
//! Controller-first navigation (lane v020-steam-deck): the D-pad and left stick move focus, A
//! submits, B cancels, the shoulders step through the order, left/right drive a focused slider, and
//! the focus ring mode follows the last input device. Headless: no window, no GPU.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use regia_app::TimeUpdateStrategy;
use regia_app::prelude::*;
use regia_platform::{ButtonState, GamepadAxis, GamepadAxisChanged, GamepadButton, GamepadButtonChanged};
use regia_ui::focus::{DefaultFocus, FocusScope, FocusState, Focusable, UiInputMode};
use regia_ui::prelude::*;
use regia_ui::widgets::Slider;
use regia_ui::{UiEvent, UiEventKind};

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

fn node(app: &mut App, rect: [f32; 4]) -> Entity {
    app.world_mut()
        .spawn((
            Focusable::default(),
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

fn focused(app: &App) -> Option<Entity> {
    app.world().resource::<FocusState>().focused
}

fn clicks(app: &mut App) -> Vec<Entity> {
    app.world_mut()
        .resource_mut::<Messages<UiEvent>>()
        .iter_current_update_messages()
        .filter(|e| e.kind == UiEventKind::Click)
        .map(|e| e.target)
        .collect()
}

#[test]
fn dpad_moves_focus_through_a_menu_and_a_b_confirm_and_back() {
    let mut app = app();
    let a = node(&mut app, [100.0, 100.0, 300.0, 60.0]);
    let b = node(&mut app, [100.0, 180.0, 300.0, 60.0]);
    let c = node(&mut app, [100.0, 260.0, 300.0, 60.0]);
    app.world_mut().entity_mut(a).insert(DefaultFocus);
    assert_eq!(*app.world().resource::<UiInputMode>(), UiInputMode::Pointer);

    // The first press focuses the default node, the next moves down.
    press(&mut app, GamepadButton::DPadDown);
    assert_eq!(focused(&app), Some(a));
    assert_eq!(*app.world().resource::<UiInputMode>(), UiInputMode::Navigation);
    press(&mut app, GamepadButton::DPadDown);
    assert_eq!(focused(&app), Some(b));
    press(&mut app, GamepadButton::DPadDown);
    assert_eq!(focused(&app), Some(c));
    press(&mut app, GamepadButton::DPadDown);
    assert_eq!(focused(&app), Some(c), "no wrap past the last item");
    press(&mut app, GamepadButton::DPadUp);
    assert_eq!(focused(&app), Some(b));

    // A clicks the focused node.
    for state in [ButtonState::Pressed, ButtonState::Released] {
        app.world_mut().write_message(GamepadButtonChanged {
            device: 1,
            button: GamepadButton::South,
            state,
            value: 1.0,
        });
        app.update();
        if state == ButtonState::Pressed {
            assert_eq!(clicks(&mut app), vec![b]);
        }
    }

    // Shoulders walk the focus order.
    press(&mut app, GamepadButton::RightShoulder);
    assert_eq!(focused(&app), Some(c));
    press(&mut app, GamepadButton::LeftShoulder);
    assert_eq!(focused(&app), Some(b));
}

#[test]
fn b_cancels_the_modal_scope_and_focus_stays_inside_it() {
    let mut app = app();
    let outside = node(&mut app, [0.0, 0.0, 100.0, 40.0]);
    let modal = app
        .world_mut()
        .spawn((
            FocusScope,
            ComputedNode {
                rect: [400.0, 300.0, 300.0, 200.0],
                scale: 1.0,
                ..Default::default()
            },
        ))
        .id();
    let ok = app
        .world_mut()
        .spawn((
            Focusable::default(),
            ComputedNode {
                rect: [420.0, 320.0, 100.0, 40.0],
                scale: 1.0,
                ..Default::default()
            },
            ChildOf(modal),
        ))
        .id();
    app.world_mut().entity_mut(ok).insert(DefaultFocus);
    press(&mut app, GamepadButton::DPadDown);
    assert_eq!(focused(&app), Some(ok));
    press(&mut app, GamepadButton::DPadUp);
    assert_ne!(focused(&app), Some(outside), "the modal traps focus");
    app.world_mut().write_message(GamepadButtonChanged {
        device: 1,
        button: GamepadButton::East,
        state: ButtonState::Pressed,
        value: 1.0,
    });
    app.update();
    assert_eq!(clicks(&mut app), vec![modal], "B activates the scope's dismissal");
}

#[test]
fn the_left_stick_navigates_and_repeats_while_held() {
    let mut app = app();
    let ids: Vec<Entity> = (0..8)
        .map(|i| node(&mut app, [100.0, 100.0 + 80.0 * i as f32, 300.0, 60.0]))
        .collect();
    app.world_mut().entity_mut(ids[0]).insert(DefaultFocus);
    let stick = |app: &mut App, y: f32| {
        app.world_mut().write_message(GamepadAxisChanged {
            device: 1,
            axis: GamepadAxis::LeftStickY,
            value: y,
        });
    };
    // Push down (platform convention: down is negative) and hold ~1 s at 60 fps.
    stick(&mut app, -1.0);
    app.update();
    assert_eq!(
        focused(&app),
        Some(ids[0]),
        "first press focuses the default node"
    );
    for _ in 0..62 {
        app.update();
    }
    let index = ids.iter().position(|e| Some(*e) == focused(&app)).unwrap();
    assert!((5..=7).contains(&index), "held stick moved focus to item {index}");
    // Releasing the stick stops the movement.
    stick(&mut app, 0.0);
    app.update();
    let at_release = focused(&app);
    for _ in 0..30 {
        app.update();
    }
    assert_eq!(focused(&app), at_release);
}

#[test]
fn left_right_changes_a_focused_slider_and_a_does_not_jump_it() {
    let mut app = app();
    let slider = app
        .world_mut()
        .spawn((
            Focusable::default(),
            DefaultFocus,
            Slider {
                value: 0.5,
                min: 0.0,
                max: 1.0,
                step: 0.1,
            },
            ComputedNode {
                rect: [100.0, 100.0, 400.0, 30.0],
                scale: 1.0,
                ..Default::default()
            },
        ))
        .id();
    press(&mut app, GamepadButton::DPadDown); // focus it
    assert_eq!(focused(&app), Some(slider));
    press(&mut app, GamepadButton::DPadRight);
    let v = app.world().get::<Slider>(slider).unwrap().value;
    assert!((v - 0.6).abs() < 1e-3, "{v}");
    press(&mut app, GamepadButton::DPadLeft);
    press(&mut app, GamepadButton::DPadLeft);
    let v = app.world().get::<Slider>(slider).unwrap().value;
    assert!((v - 0.4).abs() < 1e-3, "{v}");
    press(&mut app, GamepadButton::South);
    let v = app.world().get::<Slider>(slider).unwrap().value;
    assert!(
        (v - 0.4).abs() < 1e-3,
        "A must not jump the slider to the pointer: {v}"
    );
}

#[test]
fn the_pointer_switches_the_mode_back() {
    let mut app = app();
    let a = node(&mut app, [0.0, 0.0, 100.0, 40.0]);
    app.world_mut().entity_mut(a).insert(DefaultFocus);
    press(&mut app, GamepadButton::DPadDown);
    assert_eq!(*app.world().resource::<UiInputMode>(), UiInputMode::Navigation);
    app.world_mut().write_message(regia_platform::CursorMoved {
        window: Entity::PLACEHOLDER,
        position: (10.0, 10.0),
        timestamp: Default::default(),
    });
    app.update();
    assert_eq!(*app.world().resource::<UiInputMode>(), UiInputMode::Pointer);
}

#[test]
fn the_handheld_text_floor_enlarges_small_text_only() {
    use regia_ui::text::{Text, TextStyle, TextSystem};
    let loc = regia_ui::Localization::default();
    let small = Text::new("Settings").with_style(TextStyle {
        font_px: 8.0,
        ..Default::default()
    });
    let big = Text::new("Settings").with_style(TextStyle {
        font_px: 24.0,
        ..Default::default()
    });
    let mut sys = TextSystem::embedded_only();
    let free_small = sys.shape(&small, 1.0, None, &loc).size;
    sys.min_font_px = regia_ui::HANDHELD_MIN_TEXT_PX;
    let floored_small = sys.shape(&small, 1.0, None, &loc).size;
    let floored_big = sys.shape(&big, 1.0, None, &loc).size;
    sys.min_font_px = 0.0;
    let free_big = sys.shape(&big, 1.0, None, &loc).size;
    assert!(
        floored_small[1] >= 12.0 * 1.15,
        "8 px text is lifted to 12 px: line height {} vs {}",
        floored_small[1],
        free_small[1]
    );
    assert!(
        floored_small[0] > free_small[0] * 1.4,
        "and is wider: {floored_small:?} vs {free_small:?}"
    );
    assert_eq!(floored_big, free_big, "24 px text is untouched");
}

#[test]
fn a_game_only_layer_keeps_the_pad_from_switching_the_ui_to_navigation() {
    let mut app = app();
    app.update();
    app.world_mut().spawn(regia_ui::InputLayer {
        mode: regia_ui::LayerInputMode::GameOnly,
    });
    app.update();
    press(&mut app, GamepadButton::DPadDown);
    assert_eq!(*app.world().resource::<UiInputMode>(), UiInputMode::Pointer);
}

#[derive(Component)]
struct ArchetypeMarker;

#[test]
fn equal_score_candidates_resolve_to_the_lowest_entity_regardless_of_archetype() {
    for marker_on_first in [false, true] {
        let mut app = app();
        let left = node(&mut app, [0.0, 0.0, 100.0, 50.0]);
        let first = node(&mut app, [200.0, 0.0, 100.0, 50.0]);
        let second = node(&mut app, [200.0, 0.0, 100.0, 50.0]);
        let marked = if marker_on_first { first } else { second };
        app.world_mut().entity_mut(marked).insert(ArchetypeMarker);
        app.world_mut().entity_mut(left).insert(DefaultFocus);
        press(&mut app, GamepadButton::DPadRight);
        assert_eq!(focused(&app), Some(left));
        press(&mut app, GamepadButton::DPadRight);
        // The lowest by `Entity`'s order, which is not spawn order in bevy_ecs 0.20.
        let lowest = first.min(second);
        assert_eq!(focused(&app), Some(lowest), "marker_on_first = {marker_on_first}");
    }
}
