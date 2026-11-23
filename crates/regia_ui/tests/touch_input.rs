// SPDX-License-Identifier: MPL-2.0
//! Touch input correctness (audit fix-ui3-correctness-k, finding 1): a finger tap clicks the
//! node under the lift point (never the mouse cursor, which a pure touch path never sets), a
//! `Started`+`Ended` pair delivered in one frame is still a tap, and a finger drop lands on the
//! [`DropTarget`] under the lift point. Mouse: a press and release delivered in one frame is
//! still a click (the release defers one frame, the touch analogue of ADR-4932). Headless: no
//! window, no GPU, fixed 100 ms steps.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use regia_app::TimeUpdateStrategy;
use regia_app::prelude::*;
use regia_platform::{ButtonState, MouseButton, TouchPhase};
use regia_ui::prelude::*;
use regia_ui::{DragDropEvent, DragPayload, DragSource, DropTarget};

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
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(100)));
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

fn button(app: &mut App, rect: [f32; 4]) -> Entity {
    app.world_mut()
        .spawn((Interactable::default(), computed(rect)))
        .id()
}

fn touch(app: &mut App, id: u64, phase: TouchPhase, x: f64, y: f64) {
    app.world_mut().write_message(regia_platform::TouchInput {
        window: Entity::PLACEHOLDER,
        id,
        phase,
        position: (x, y),
        timestamp: Default::default(),
    });
}

fn ui_events(app: &mut App) -> Vec<UiEvent> {
    app.world_mut()
        .resource_mut::<Messages<UiEvent>>()
        .drain()
        .collect()
}

fn clicks(target: Entity, events: &[UiEvent]) -> usize {
    events
        .iter()
        .filter(|e| e.target == target && e.kind == UiEventKind::Click)
        .count()
}

#[test]
fn finger_tap_clicks_the_node_under_the_lift_point() {
    let mut app = app();
    let b = button(&mut app, [0.0, 0.0, 100.0, 100.0]);
    touch(&mut app, 1, TouchPhase::Started, 50.0, 50.0);
    app.update();
    touch(&mut app, 1, TouchPhase::Ended, 50.0, 50.0);
    app.update();
    // The lift is deferred one frame so the release edge hit-tests at the lift position
    // instead of falling back to the (never set) mouse cursor.
    app.update();
    let events = ui_events(&mut app);
    assert_eq!(clicks(b, &events), 1, "a tap is a click: {events:?}");
}

#[test]
fn started_and_ended_in_one_frame_still_clicks() {
    let mut app = app();
    let b = button(&mut app, [0.0, 0.0, 100.0, 100.0]);
    touch(&mut app, 1, TouchPhase::Started, 50.0, 50.0);
    touch(&mut app, 1, TouchPhase::Ended, 50.0, 50.0);
    app.update();
    app.update();
    let events = ui_events(&mut app);
    assert_eq!(clicks(b, &events), 1, "a same-frame tap is a click: {events:?}");
}

/// rc1 UI-text finding 3: a mouse press and release delivered in the SAME frame batch used to
/// be lost — last-wins batching left the button up, so the interaction pass saw neither the
/// press nor the release edge. The release now defers one frame, the same deferral a
/// same-frame touch tap gets (the ADR-4932 analog for mouse buttons).
#[test]
fn mouse_press_and_release_in_one_frame_still_clicks() {
    let mut app = app();
    let b = button(&mut app, [0.0, 0.0, 100.0, 100.0]);
    app.world_mut().write_message(regia_platform::CursorMoved {
        window: Entity::PLACEHOLDER,
        position: (50.0, 50.0),
        timestamp: Default::default(),
    });
    for state in [ButtonState::Pressed, ButtonState::Released] {
        app.world_mut().write_message(regia_platform::MouseButtonInput {
            window: Entity::PLACEHOLDER,
            button: MouseButton::Left,
            state,
            timestamp: Default::default(),
        });
    }
    app.update();
    app.update();
    let events = ui_events(&mut app);
    assert_eq!(clicks(b, &events), 1, "a same-frame click is a click: {events:?}");
}

/// The same-frame deferral covers the secondary and middle buttons, whose click path
/// (`secondary_pressed`/`middle_pressed`) had the identical loss.
#[test]
fn mouse_press_and_release_in_one_frame_still_context_clicks() {
    let mut app = app();
    let b = button(&mut app, [0.0, 0.0, 100.0, 100.0]);
    app.world_mut().write_message(regia_platform::CursorMoved {
        window: Entity::PLACEHOLDER,
        position: (50.0, 50.0),
        timestamp: Default::default(),
    });
    for state in [ButtonState::Pressed, ButtonState::Released] {
        app.world_mut().write_message(regia_platform::MouseButtonInput {
            window: Entity::PLACEHOLDER,
            button: MouseButton::Right,
            state,
            timestamp: Default::default(),
        });
    }
    app.update();
    app.update();
    let events = ui_events(&mut app);
    let context = events
        .iter()
        .filter(|e| e.target == b && e.kind == UiEventKind::ContextClick)
        .count();
    assert_eq!(context, 1, "a same-frame right click is a click: {events:?}");
}

#[test]
fn finger_lift_away_from_the_press_does_not_click() {
    let mut app = app();
    let b = button(&mut app, [0.0, 0.0, 100.0, 100.0]);
    touch(&mut app, 1, TouchPhase::Started, 50.0, 50.0);
    app.update();
    touch(&mut app, 1, TouchPhase::Ended, 300.0, 300.0);
    app.update();
    app.update();
    let events = ui_events(&mut app);
    assert_eq!(
        clicks(b, &events),
        0,
        "lifting outside is not a click: {events:?}"
    );
}

#[test]
fn finger_drag_drops_on_the_target_under_the_lift_point() {
    let mut app = app();
    let item = app
        .world_mut()
        .spawn((
            Interactable::default(),
            computed([0.0, 0.0, 40.0, 40.0]),
            DragSource::new(DragPayload::new("item", UiValue::Index(7))),
        ))
        .id();
    let slot = app
        .world_mut()
        .spawn((
            Interactable::default(),
            computed([200.0, 0.0, 80.0, 80.0]),
            DropTarget::accepting(["item"]),
        ))
        .id();

    touch(&mut app, 1, TouchPhase::Started, 20.0, 20.0);
    app.update();
    touch(&mut app, 1, TouchPhase::Moved, 26.0, 20.0);
    app.update();
    touch(&mut app, 1, TouchPhase::Moved, 240.0, 40.0);
    app.update();
    touch(&mut app, 1, TouchPhase::Ended, 240.0, 40.0);
    app.update();
    app.update();

    let events: Vec<DragDropEvent> = app
        .world_mut()
        .resource_mut::<Messages<DragDropEvent>>()
        .drain()
        .collect();
    assert!(
        events.contains(&DragDropEvent::Dropped {
            source: item,
            target: slot,
            payload: DragPayload::new("item", UiValue::Index(7)),
        }),
        "the finger drop lands on the target under the lift point: {events:?}"
    );
}
