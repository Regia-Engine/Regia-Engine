// SPDX-License-Identifier: MPL-2.0
//! Input events and input layers (ADR-273 lane 4, ADR-293): buttons, modifiers, double and
//! context clicks, scroll, long press and repeat at fixed steps, and the layer stack. Headless:
//! no window, no GPU, game time advances by a fixed 100 ms per update.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use regia_app::TimeUpdateStrategy;
use regia_app::prelude::*;
use regia_platform::{ButtonState, MouseButton};
use regia_ui::focus::{DefaultFocus, FocusScope, FocusState, Focusable};
use regia_ui::layers::{InputLayers, LayerError, pop_layer, push_layer};
use regia_ui::prelude::*;
use regia_ui::{
    InputLayer, LayerInputMode, LongPressable, Pressed, RepeatOnHold, UiButton, UiEvent, UiEventKind,
    UiInputState, UiPointer,
};

const STEP_MS: u64 = 100;

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
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(STEP_MS)));
    app
}

fn computed(rect: [f32; 4]) -> ComputedNode {
    ComputedNode {
        rect,
        clip_rect: rect,
        scale: 1.0,
        ..Default::default()
    }
}

/// A pointer-interactable, focusable node.
fn node(app: &mut App, rect: [f32; 4], parent: Option<Entity>) -> Entity {
    let id = app
        .world_mut()
        .spawn((Interactable::default(), Focusable::default(), computed(rect)))
        .id();
    if let Some(parent) = parent {
        // Children draw above their parent, as layout assigns.
        app.world_mut().get_mut::<ComputedNode>(id).unwrap().z = 1.0;
        app.world_mut().entity_mut(id).insert(ChildOf(parent));
    }
    id
}

/// A layer root: interactable (the scrim) and a layer.
fn layer(app: &mut App, rect: [f32; 4], mode: LayerInputMode) -> Entity {
    app.world_mut()
        .spawn((Interactable::default(), computed(rect), InputLayer { mode }))
        .id()
}

fn drain(app: &mut App) -> Vec<UiEvent> {
    app.world_mut()
        .resource_mut::<Messages<UiEvent>>()
        .iter_current_update_messages()
        .cloned()
        .collect()
}

fn move_to(app: &mut App, x: f64, y: f64) -> Vec<UiEvent> {
    app.world_mut().write_message(regia_platform::CursorMoved {
        window: Entity::PLACEHOLDER,
        position: (x, y),
        timestamp: Default::default(),
    });
    app.update();
    drain(app)
}

fn button(app: &mut App, button: MouseButton, state: ButtonState) -> Vec<UiEvent> {
    app.world_mut().write_message(regia_platform::MouseButtonInput {
        window: Entity::PLACEHOLDER,
        button,
        state,
        timestamp: Default::default(),
    });
    app.update();
    drain(app)
}

/// Move, press, release: three updates. Returns every event produced.
fn click_with(app: &mut App, x: f64, y: f64, which: MouseButton) -> Vec<UiEvent> {
    let mut all = move_to(app, x, y);
    all.extend(button(app, which, ButtonState::Pressed));
    all.extend(button(app, which, ButtonState::Released));
    all
}

fn click(app: &mut App, x: f64, y: f64) -> Vec<UiEvent> {
    click_with(app, x, y, MouseButton::Left)
}

fn of_kind(events: &[UiEvent], target: Entity, kind: &UiEventKind) -> usize {
    events
        .iter()
        .filter(|e| e.target == target && &e.kind == kind)
        .count()
}

fn focused(app: &App) -> Option<Entity> {
    app.world().resource::<FocusState>().focused
}

fn layer_mode(app: &App) -> LayerInputMode {
    app.world().resource::<UiInputState>().layer_mode
}

fn depth(app: &App) -> usize {
    app.world().resource::<InputLayers>().depth()
}

#[test]
fn modal_over_modal_over_page_only_the_top_gets_input_and_pop_restores_focus() {
    let mut app = app();
    let page = app.world_mut().spawn((computed([0.0, 0.0, 800.0, 600.0]),)).id();
    let a = node(&mut app, [0.0, 0.0, 100.0, 40.0], Some(page));
    app.world_mut().resource_mut::<FocusState>().focused = Some(a);
    app.update();
    assert_eq!(layer_mode(&app), LayerInputMode::UiAndGame, "no layer yet");
    assert_eq!(of_kind(&click(&mut app, 50.0, 20.0), a, &UiEventKind::Click), 1);

    // First modal: UI only. Its default focus takes the focus.
    let m1 = layer(&mut app, [200.0, 0.0, 300.0, 300.0], LayerInputMode::UiOnly);
    let b1 = node(&mut app, [210.0, 10.0, 80.0, 40.0], Some(m1));
    app.world_mut().entity_mut(b1).insert(DefaultFocus);
    app.update();
    assert_eq!(depth(&app), 1);
    assert_eq!(focused(&app), Some(b1));
    assert_eq!(layer_mode(&app), LayerInputMode::UiOnly);
    assert_eq!(
        of_kind(&click(&mut app, 50.0, 20.0), a, &UiEventKind::Click),
        0,
        "the page behind a modal gets no click"
    );
    assert_eq!(of_kind(&click(&mut app, 250.0, 30.0), b1, &UiEventKind::Click), 1);

    // Second modal over the first.
    let m2 = layer(&mut app, [250.0, 100.0, 100.0, 100.0], LayerInputMode::UiAndGame);
    let b2 = node(&mut app, [260.0, 110.0, 60.0, 30.0], Some(m2));
    app.world_mut().entity_mut(b2).insert(DefaultFocus);
    app.update();
    assert_eq!(depth(&app), 2);
    assert_eq!(focused(&app), Some(b2));
    assert_eq!(layer_mode(&app), LayerInputMode::UiAndGame);
    assert_eq!(
        of_kind(&click(&mut app, 250.0, 30.0), b1, &UiEventKind::Click),
        0,
        "the first modal is covered by the second"
    );
    assert_eq!(
        of_kind(&click(&mut app, 220.0, 250.0), m1, &UiEventKind::Click),
        0,
        "even the first modal's own scrim"
    );
    assert_eq!(
        of_kind(&click(&mut app, 270.0, 120.0), b2, &UiEventKind::Click),
        1
    );

    // Pop the second: input and focus return to the first, and so does its mode.
    app.world_mut().entity_mut(m2).remove::<InputLayer>();
    app.update();
    assert_eq!(depth(&app), 1);
    assert_eq!(
        focused(&app),
        Some(b1),
        "focus returns to the first modal's element"
    );
    assert_eq!(
        layer_mode(&app),
        LayerInputMode::UiOnly,
        "input mode restored on pop"
    );
    assert_eq!(of_kind(&click(&mut app, 250.0, 30.0), b1, &UiEventKind::Click), 1);

    // Pop the first by despawning it: the page is live again and has its focus back.
    app.world_mut().entity_mut(m1).despawn();
    app.update();
    assert_eq!(depth(&app), 0);
    assert_eq!(focused(&app), Some(a));
    assert_eq!(layer_mode(&app), LayerInputMode::UiAndGame);
    assert_eq!(of_kind(&click(&mut app, 50.0, 20.0), a, &UiEventKind::Click), 1);
}

#[test]
fn a_game_only_layer_hands_pointer_input_to_the_game() {
    let mut app = app();
    let a = node(&mut app, [0.0, 0.0, 100.0, 40.0], None);
    let hud = layer(&mut app, [0.0, 0.0, 800.0, 600.0], LayerInputMode::GameOnly);
    app.update();
    assert_eq!(layer_mode(&app), LayerInputMode::GameOnly);
    let events = click(&mut app, 50.0, 20.0);
    assert_eq!(of_kind(&events, a, &UiEventKind::Click), 0);
    assert_eq!(of_kind(&events, hud, &UiEventKind::Click), 0);
}

#[test]
fn an_open_modal_gets_a_layer_and_closing_it_removes_it() {
    let mut app = app();
    let page_button = node(&mut app, [0.0, 0.0, 100.0, 40.0], None);
    app.world_mut().resource_mut::<FocusState>().focused = Some(page_button);
    let modal = app
        .world_mut()
        .spawn((
            Modal::default(),
            FocusScope,
            Interactable::default(),
            computed([200.0, 0.0, 300.0, 300.0]),
        ))
        .id();
    let ok = node(&mut app, [210.0, 10.0, 80.0, 40.0], Some(modal));
    app.update();
    assert_eq!(depth(&app), 1, "the Modal widget API still opens a layer");
    assert_eq!(focused(&app), Some(ok));
    assert_eq!(
        of_kind(&click(&mut app, 50.0, 20.0), page_button, &UiEventKind::Click),
        0
    );
    // A click on the scrim closes it (Modal's existing behaviour), then the layer goes.
    let events = click(&mut app, 400.0, 250.0);
    assert_eq!(of_kind(&events, modal, &UiEventKind::Click), 1);
    assert!(!app.world().get::<Modal>(modal).unwrap().open);
    app.update();
    assert_eq!(depth(&app), 0);
    assert_eq!(
        focused(&app),
        Some(page_button),
        "focus restored after the modal closed"
    );
}

#[test]
fn push_and_pop_through_the_world_helpers() {
    let mut app = app();
    app.update();
    let root = app
        .world_mut()
        .spawn((Interactable::default(), computed([0.0, 0.0, 10.0, 10.0])))
        .id();
    assert_eq!(pop_layer(app.world_mut(), None), Err(LayerError::NoLayers));
    assert_eq!(pop_layer(app.world_mut(), Some(root)), Err(LayerError::NotALayer));
    push_layer(app.world_mut(), root, LayerInputMode::UiOnly).unwrap();
    assert_eq!(depth(&app), 1, "applied at once, before the next frame");
    assert_eq!(layer_mode(&app), LayerInputMode::UiOnly);
    assert_eq!(
        push_layer(app.world_mut(), root, LayerInputMode::UiOnly),
        Err(LayerError::AlreadyLayer)
    );
    assert_eq!(pop_layer(app.world_mut(), None), Ok(root));
    assert_eq!(depth(&app), 0);
    assert_eq!(layer_mode(&app), LayerInputMode::UiAndGame);
}

#[test]
fn two_clicks_inside_the_window_are_one_double_click_and_outside_are_two_clicks() {
    let mut app = app();
    let target = node(&mut app, [0.0, 0.0, 100.0, 40.0], None);
    // Each click is 3 updates (300 ms of game time); the second press follows 100 ms later,
    // so the clicks are 300 ms apart, inside the 400 ms window.
    let mut events = click(&mut app, 50.0, 20.0);
    events.extend(click(&mut app, 50.0, 20.0));
    assert_eq!(
        of_kind(&events, target, &UiEventKind::Click),
        2,
        "Click keeps its meaning"
    );
    assert_eq!(of_kind(&events, target, &UiEventKind::DoubleClick), 1);
    let counts: Vec<u8> = events
        .iter()
        .filter(|e| e.kind == UiEventKind::Click)
        .map(|e| e.click_count)
        .collect();
    assert_eq!(counts, vec![1, 2]);

    // Wait out the window (1 s), then click twice more, 3 s apart.
    for _ in 0..10 {
        app.update();
    }
    let mut events = click(&mut app, 50.0, 20.0);
    for _ in 0..10 {
        app.update();
    }
    events.extend(click(&mut app, 50.0, 20.0));
    assert_eq!(of_kind(&events, target, &UiEventKind::Click), 2);
    assert_eq!(of_kind(&events, target, &UiEventKind::DoubleClick), 0);
    assert!(
        events
            .iter()
            .filter(|e| e.kind == UiEventKind::Click)
            .all(|e| e.click_count == 1)
    );
}

#[test]
fn ui_timers_follow_real_time_so_a_paused_game_still_separates_clicks() {
    let mut app = app();
    let target = node(&mut app, [0.0, 0.0, 100.0, 40.0], None);
    app.world_mut().resource_mut::<Time>().paused = true;
    let mut events = click(&mut app, 50.0, 20.0);
    // 3 s of real time pass while gameplay time is frozen.
    for _ in 0..30 {
        app.update();
    }
    events.extend(click(&mut app, 50.0, 20.0));
    assert_eq!(of_kind(&events, target, &UiEventKind::Click), 2);
    assert_eq!(of_kind(&events, target, &UiEventKind::DoubleClick), 0);
}

#[test]
fn clicks_on_different_nodes_do_not_count_as_a_double_click() {
    let mut app = app();
    let left = node(&mut app, [0.0, 0.0, 100.0, 40.0], None);
    let right = node(&mut app, [200.0, 0.0, 100.0, 40.0], None);
    let mut events = click(&mut app, 50.0, 20.0);
    events.extend(click(&mut app, 250.0, 20.0));
    assert_eq!(of_kind(&events, left, &UiEventKind::Click), 1);
    assert_eq!(of_kind(&events, right, &UiEventKind::Click), 1);
    assert_eq!(
        events
            .iter()
            .filter(|e| e.kind == UiEventKind::DoubleClick)
            .count(),
        0
    );
}

#[test]
fn secondary_and_middle_buttons_send_context_and_middle_clicks_not_clicks() {
    let mut app = app();
    let target = node(&mut app, [0.0, 0.0, 100.0, 40.0], None);
    let events = click_with(&mut app, 50.0, 20.0, MouseButton::Right);
    assert_eq!(of_kind(&events, target, &UiEventKind::ContextClick), 1);
    assert_eq!(
        of_kind(&events, target, &UiEventKind::Click),
        0,
        "no primary click"
    );
    let ctx = events
        .iter()
        .find(|e| e.kind == UiEventKind::ContextClick)
        .unwrap();
    assert_eq!(ctx.button, UiButton::Secondary);

    let events = click_with(&mut app, 50.0, 20.0, MouseButton::Middle);
    assert_eq!(of_kind(&events, target, &UiEventKind::MiddleClick), 1);
    assert_eq!(of_kind(&events, target, &UiEventKind::Click), 0);

    // Released away from the node: no context click.
    move_to(&mut app, 50.0, 20.0);
    button(&mut app, MouseButton::Right, ButtonState::Pressed);
    move_to(&mut app, 500.0, 500.0);
    let events = button(&mut app, MouseButton::Right, ButtonState::Released);
    assert_eq!(
        events
            .iter()
            .filter(|e| e.kind == UiEventKind::ContextClick)
            .count(),
        0
    );
}

#[test]
fn modifiers_ride_on_the_event() {
    let mut app = app();
    let target = node(&mut app, [0.0, 0.0, 100.0, 40.0], None);
    app.world_mut().write_message(regia_platform::KeyboardInput {
        window: Entity::PLACEHOLDER,
        physical_key: "ShiftLeft".into(),
        logical_key: "Shift".into(),
        state: ButtonState::Pressed,
        repeat: false,
        timestamp: Default::default(),
    });
    app.update();
    let events = click(&mut app, 50.0, 20.0);
    let click = events
        .iter()
        .find(|e| e.target == target && e.kind == UiEventKind::Click)
        .unwrap();
    assert!(click.modifiers.shift && !click.modifiers.ctrl);
    assert_eq!(click.button, UiButton::Primary);
}

#[test]
fn the_wheel_over_a_node_sends_scroll() {
    let mut app = app();
    let target = node(&mut app, [0.0, 0.0, 100.0, 40.0], None);
    move_to(&mut app, 50.0, 20.0);
    app.world_mut().write_message(regia_platform::MouseWheel {
        window: Entity::PLACEHOLDER,
        delta: (0.0, -3.0),
        unit: regia_platform::WheelUnit::Pixel,
        timestamp: Default::default(),
    });
    app.update();
    let events = drain(&mut app);
    let scroll = events
        .iter()
        .find(|e| e.target == target)
        .filter(|e| matches!(e.kind, UiEventKind::Scroll(_)));
    assert_eq!(
        scroll.map(|e| e.kind.clone()),
        Some(UiEventKind::Scroll([0.0, -3.0]))
    );
}

#[test]
fn long_press_fires_once_at_its_delay_and_the_release_is_not_a_click() {
    let mut app = app();
    let target = node(&mut app, [0.0, 0.0, 100.0, 40.0], None);
    app.world_mut()
        .entity_mut(target)
        .insert(LongPressable { delay: 0.45 });
    move_to(&mut app, 50.0, 20.0);
    button(&mut app, MouseButton::Left, ButtonState::Pressed);
    // Held updates 1..=3 are 0.1, 0.2, 0.3 s: nothing yet.
    let mut long = 0;
    for _ in 0..3 {
        app.update();
        long += of_kind(&drain(&mut app), target, &UiEventKind::LongPress);
    }
    assert_eq!(long, 0);
    // The fifth held update is 0.5 s: exactly one long press over a long hold.
    for _ in 0..20 {
        app.update();
        long += of_kind(&drain(&mut app), target, &UiEventKind::LongPress);
    }
    assert_eq!(long, 1);
    let events = button(&mut app, MouseButton::Left, ButtonState::Released);
    assert_eq!(of_kind(&events, target, &UiEventKind::Click), 0);
}

#[test]
fn repeat_on_hold_counts_at_fixed_steps_and_a_plain_press_still_clicks() {
    let mut app = app();
    let target = node(&mut app, [0.0, 0.0, 100.0, 40.0], None);
    app.world_mut().entity_mut(target).insert(RepeatOnHold {
        delay: 0.35,
        interval: 0.25,
    });
    move_to(&mut app, 50.0, 20.0);
    button(&mut app, MouseButton::Left, ButtonState::Pressed);
    // Held 1.0 s in 100 ms steps: repeats at 0.4, 0.7 and 1.0 s.
    let mut repeats = Vec::new();
    for _ in 0..10 {
        app.update();
        for e in drain(&mut app) {
            if let UiEventKind::Repeat(n) = e.kind {
                repeats.push(n);
            }
        }
    }
    assert_eq!(repeats, vec![1, 2, 3]);
    let events = button(&mut app, MouseButton::Left, ButtonState::Released);
    assert_eq!(
        of_kind(&events, target, &UiEventKind::Click),
        0,
        "repeats replace the click"
    );

    // A short press on the same node is an ordinary click.
    let events = click(&mut app, 50.0, 20.0);
    assert_eq!(of_kind(&events, target, &UiEventKind::Click), 1);
    assert!(events.iter().all(|e| !matches!(e.kind, UiEventKind::Repeat(_))));
}

#[test]
fn replacing_a_modal_in_one_frame_keeps_the_page_focus_for_the_next_pop() {
    let mut app = app();
    let a = node(&mut app, [0.0, 0.0, 100.0, 40.0], None);
    app.world_mut().resource_mut::<FocusState>().focused = Some(a);
    app.update();

    let m1 = layer(&mut app, [200.0, 0.0, 300.0, 300.0], LayerInputMode::UiOnly);
    let b1 = node(&mut app, [210.0, 10.0, 80.0, 40.0], Some(m1));
    app.world_mut().entity_mut(b1).insert(DefaultFocus);
    app.update();
    assert_eq!(focused(&app), Some(b1));

    // Close the first modal and open the second in the same frame.
    app.world_mut().entity_mut(m1).despawn();
    let m2 = layer(&mut app, [200.0, 0.0, 300.0, 300.0], LayerInputMode::UiOnly);
    let b2 = node(&mut app, [210.0, 10.0, 80.0, 40.0], Some(m2));
    app.world_mut().entity_mut(b2).insert(DefaultFocus);
    app.update();
    assert_eq!(depth(&app), 1);
    assert_eq!(focused(&app), Some(b2));

    app.world_mut().entity_mut(m2).despawn();
    app.update();
    assert_eq!(depth(&app), 0);
    assert_eq!(focused(&app), Some(a), "the page's focus survives the swap");
}

#[test]
fn removing_a_covered_layer_hands_its_saved_focus_to_the_layer_above() {
    let mut app = app();
    let a = node(&mut app, [0.0, 0.0, 100.0, 40.0], None);
    app.world_mut().resource_mut::<FocusState>().focused = Some(a);
    app.update();
    let m1 = layer(&mut app, [200.0, 0.0, 300.0, 300.0], LayerInputMode::UiOnly);
    let b1 = node(&mut app, [210.0, 10.0, 80.0, 40.0], Some(m1));
    app.world_mut().entity_mut(b1).insert(DefaultFocus);
    app.update();
    let m2 = layer(&mut app, [250.0, 100.0, 100.0, 100.0], LayerInputMode::UiOnly);
    let b2 = node(&mut app, [260.0, 110.0, 60.0, 30.0], Some(m2));
    app.world_mut().entity_mut(b2).insert(DefaultFocus);
    app.update();
    assert_eq!(depth(&app), 2);

    // Remove the lower layer: focus stays on the top one, and popping that returns to the page.
    app.world_mut().entity_mut(m1).remove::<InputLayer>();
    app.update();
    assert_eq!(depth(&app), 1);
    assert_eq!(focused(&app), Some(b2));
    app.world_mut().entity_mut(m2).remove::<InputLayer>();
    app.update();
    assert_eq!(focused(&app), Some(a));
}

#[test]
fn a_drag_ends_when_a_layer_covers_the_pressed_node() {
    let mut app = app();
    let s = node(&mut app, [0.0, 0.0, 100.0, 40.0], None);
    move_to(&mut app, 10.0, 20.0);
    button(&mut app, MouseButton::Left, ButtonState::Pressed);
    let drag = move_to(&mut app, 30.0, 20.0);
    assert_eq!(of_kind(&drag, s, &UiEventKind::DragStart), 1);

    let m = layer(&mut app, [200.0, 0.0, 300.0, 300.0], LayerInputMode::UiOnly);
    app.update();
    assert_eq!(depth(&app), 1);
    assert!(app.world().get::<Pressed>(s).is_none(), "the press is cancelled");
    assert!(app.world().resource::<UiPointer>().pressed.is_none());

    let later = move_to(&mut app, 50.0, 20.0);
    assert!(
        later.iter().all(|e| e.target != s),
        "the covered node gets no more drag events"
    );
    let released = button(&mut app, MouseButton::Left, ButtonState::Released);
    assert!(released.iter().all(|e| e.target != s && e.target != m));
}

#[test]
fn a_press_that_began_on_nothing_does_not_arm_the_node_it_drags_onto() {
    let mut app = app();
    let n = node(&mut app, [0.0, 0.0, 100.0, 40.0], None);
    for which in [MouseButton::Left, MouseButton::Right, MouseButton::Middle] {
        move_to(&mut app, 500.0, 500.0);
        button(&mut app, which, ButtonState::Pressed);
        move_to(&mut app, 50.0, 20.0);
        let events = button(&mut app, which, ButtonState::Released);
        assert!(
            events.iter().all(|e| !matches!(
                e.kind,
                UiEventKind::Click | UiEventKind::ContextClick | UiEventKind::MiddleClick
            )),
            "{which:?}: {events:?}"
        );
        assert_eq!(of_kind(&events, n, &UiEventKind::Click), 0);
    }
    // A press that starts on the node still clicks.
    assert_eq!(of_kind(&click(&mut app, 50.0, 20.0), n, &UiEventKind::Click), 1);
}
