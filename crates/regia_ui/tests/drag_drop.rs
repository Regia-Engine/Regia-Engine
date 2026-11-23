// SPDX-License-Identifier: MPL-2.0
//! Drag and drop between widgets (ADR-517): payload types, hover targets, drop, and every cancel
//! path. Headless: no window, no GPU, game time advances by a fixed 100 ms per update.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use regia_app::TimeUpdateStrategy;
use regia_app::prelude::*;
use regia_platform::{ButtonState, MouseButton};
use regia_ui::layers::{pop_layer, push_layer};
use regia_ui::prelude::*;
use regia_ui::{
    DragActive, DragCancelReason, DragDrop, DragDropEvent, DragPayload, DragSource, DropHover, DropTarget,
    LayerInputMode, cancel_drag,
};

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

fn computed(rect: [f32; 4], z: f32) -> ComputedNode {
    ComputedNode {
        rect,
        clip_rect: rect,
        scale: 1.0,
        z,
        ..Default::default()
    }
}

fn node(app: &mut App, rect: [f32; 4]) -> Entity {
    app.world_mut()
        .spawn((Interactable::default(), computed(rect, 0.0)))
        .id()
}

/// A draggable item carrying `kind`.
fn source(app: &mut App, rect: [f32; 4], kind: &str, id: usize) -> Entity {
    let e = node(app, rect);
    app.world_mut()
        .entity_mut(e)
        .insert(DragSource::new(DragPayload::new(kind, UiValue::Index(id))));
    e
}

fn target(app: &mut App, rect: [f32; 4], accepts: &[&str]) -> Entity {
    let e = node(app, rect);
    app.world_mut()
        .entity_mut(e)
        .insert(DropTarget::accepting(accepts.iter().copied()));
    e
}

fn drain(app: &mut App) -> Vec<DragDropEvent> {
    app.world_mut()
        .resource_mut::<Messages<DragDropEvent>>()
        .iter_current_update_messages()
        .cloned()
        .collect()
}

fn move_to(app: &mut App, x: f64, y: f64) -> Vec<DragDropEvent> {
    app.world_mut().write_message(regia_platform::CursorMoved {
        window: Entity::PLACEHOLDER,
        position: (x, y),
        timestamp: Default::default(),
    });
    app.update();
    drain(app)
}

fn left(app: &mut App, state: ButtonState) -> Vec<DragDropEvent> {
    app.world_mut().write_message(regia_platform::MouseButtonInput {
        window: Entity::PLACEHOLDER,
        button: MouseButton::Left,
        state,
        timestamp: Default::default(),
    });
    app.update();
    drain(app)
}

fn key(app: &mut App, logical: &str) -> Vec<DragDropEvent> {
    app.world_mut().write_message(regia_platform::KeyboardInput {
        window: Entity::PLACEHOLDER,
        physical_key: logical.to_owned(),
        logical_key: logical.to_owned(),
        state: ButtonState::Pressed,
        repeat: false,
        timestamp: Default::default(),
    });
    app.update();
    drain(app)
}

/// Presses on `(x, y)` and moves to `(to_x, to_y)` in two steps so the drag starts. Returns the
/// events of all three updates.
fn grab_and_move(app: &mut App, from: (f64, f64), to: (f64, f64)) -> Vec<DragDropEvent> {
    let mut all = move_to(app, from.0, from.1);
    all.extend(left(app, ButtonState::Pressed));
    all.extend(move_to(app, from.0 + 6.0, from.1));
    all.extend(move_to(app, to.0, to.1));
    all
}

fn active(app: &App) -> bool {
    app.world().resource::<DragDrop>().active().is_some()
}

#[test]
fn drop_on_an_accepting_target_delivers_the_payload_once() {
    let mut app = app();
    let item = source(&mut app, [0.0, 0.0, 40.0, 40.0], "item", 7);
    let slot = target(&mut app, [200.0, 0.0, 80.0, 80.0], &["item"]);

    let events = grab_and_move(&mut app, (20.0, 20.0), (240.0, 40.0));
    assert!(events.contains(&DragDropEvent::Started {
        source: item,
        payload: DragPayload::new("item", UiValue::Index(7)),
    }));
    assert!(events.contains(&DragDropEvent::HoverEnter {
        source: item,
        target: slot,
        accepted: true,
    }));
    assert!(active(&app));
    assert!(app.world().get::<DragActive>(item).is_some());
    assert_eq!(
        app.world().get::<DropHover>(slot),
        Some(&DropHover { accepted: true })
    );
    let op = app.world().resource::<DragDrop>().active().unwrap().clone();
    assert_eq!(
        op.grab_offset,
        [26.0, 20.0],
        "grab offset is measured at the drag start"
    );
    assert_eq!(op.ghost_origin(), Some([214.0, 20.0]));

    let events = left(&mut app, ButtonState::Released);
    let dropped: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, DragDropEvent::Dropped { .. }))
        .collect();
    assert_eq!(
        dropped,
        [&DragDropEvent::Dropped {
            source: item,
            target: slot,
            payload: DragPayload::new("item", UiValue::Index(7)),
        }]
    );
    assert!(!active(&app));
    assert!(app.world().get::<DragActive>(item).is_none());
    assert!(app.world().get::<DropHover>(slot).is_none());
}

#[test]
fn a_target_that_rejects_the_kind_shows_no_drop_and_cancels_on_release() {
    let mut app = app();
    let item = source(&mut app, [0.0, 0.0, 40.0, 40.0], "file", 1);
    let slot = target(&mut app, [200.0, 0.0, 80.0, 80.0], &["item"]);

    let events = grab_and_move(&mut app, (20.0, 20.0), (240.0, 40.0));
    assert!(events.contains(&DragDropEvent::HoverEnter {
        source: item,
        target: slot,
        accepted: false,
    }));
    assert_eq!(
        app.world().get::<DropHover>(slot),
        Some(&DropHover { accepted: false })
    );
    let events = left(&mut app, ButtonState::Released);
    assert!(events.contains(&DragDropEvent::Cancelled {
        source: item,
        reason: DragCancelReason::Rejected,
    }));
    assert!(!events.iter().any(|e| matches!(e, DragDropEvent::Dropped { .. })));
    assert!(!active(&app));
}

#[test]
fn release_over_empty_space_cancels_with_no_target() {
    let mut app = app();
    let item = source(&mut app, [0.0, 0.0, 40.0, 40.0], "item", 1);
    target(&mut app, [200.0, 0.0, 80.0, 80.0], &[]);
    grab_and_move(&mut app, (20.0, 20.0), (500.0, 400.0));
    assert!(active(&app));
    let events = left(&mut app, ButtonState::Released);
    assert_eq!(
        events,
        [DragDropEvent::Cancelled {
            source: item,
            reason: DragCancelReason::NoTarget,
        }]
    );
}

#[test]
fn escape_cancels_and_the_release_afterwards_drops_nothing() {
    let mut app = app();
    let item = source(&mut app, [0.0, 0.0, 40.0, 40.0], "item", 1);
    let slot = target(&mut app, [200.0, 0.0, 80.0, 80.0], &["item"]);
    grab_and_move(&mut app, (20.0, 20.0), (240.0, 40.0));
    let events = key(&mut app, "Escape");
    assert_eq!(
        events,
        [
            DragDropEvent::HoverLeave {
                source: item,
                target: slot,
            },
            DragDropEvent::Cancelled {
                source: item,
                reason: DragCancelReason::Escape,
            },
        ]
    );
    assert!(app.world().get::<DragActive>(item).is_none());
    assert!(app.world().get::<DropHover>(slot).is_none());
    // The button is still down; moving on and releasing must not drop.
    let mut rest = move_to(&mut app, 245.0, 45.0);
    rest.extend(left(&mut app, ButtonState::Released));
    assert!(rest.is_empty(), "nothing after a cancel: {rest:?}");
}

#[test]
fn the_nearest_ancestor_target_receives_drops_on_its_children() {
    let mut app = app();
    let item = source(&mut app, [0.0, 0.0, 40.0, 40.0], "item", 3);
    let list = target(&mut app, [200.0, 0.0, 100.0, 200.0], &["item"]);
    let row = app
        .world_mut()
        .spawn((
            Interactable::default(),
            computed([200.0, 0.0, 100.0, 30.0], 1.0),
            ChildOf(list),
        ))
        .id();
    grab_and_move(&mut app, (20.0, 20.0), (220.0, 10.0));
    let op = app.world().resource::<DragDrop>().active().unwrap();
    assert_eq!(
        op.hover,
        Some((list, true)),
        "the row resolves to the list, not itself"
    );
    assert_ne!(op.hover.map(|h| h.0), Some(row));
    let events = left(&mut app, ButtonState::Released);
    assert!(events.contains(&DragDropEvent::Dropped {
        source: item,
        target: list,
        payload: DragPayload::new("item", UiValue::Index(3)),
    }));
}

#[test]
fn moving_between_targets_leaves_one_and_enters_the_other() {
    let mut app = app();
    let item = source(&mut app, [0.0, 0.0, 40.0, 40.0], "item", 1);
    let a = target(&mut app, [100.0, 0.0, 60.0, 60.0], &["item"]);
    let b = target(&mut app, [200.0, 0.0, 60.0, 60.0], &["other"]);
    grab_and_move(&mut app, (20.0, 20.0), (120.0, 20.0));
    let events = move_to(&mut app, 220.0, 20.0);
    assert_eq!(
        events,
        [
            DragDropEvent::HoverLeave {
                source: item,
                target: a,
            },
            DragDropEvent::HoverEnter {
                source: item,
                target: b,
                accepted: false,
            },
        ]
    );
    assert!(app.world().get::<DropHover>(a).is_none());
    assert!(app.world().get::<DropHover>(b).is_some());
}

#[test]
fn a_target_inside_the_source_subtree_is_not_a_drop_site() {
    let mut app = app();
    let item = source(&mut app, [0.0, 0.0, 100.0, 100.0], "item", 1);
    app.world_mut().spawn((
        Interactable::default(),
        DropTarget::default(),
        computed([60.0, 60.0, 40.0, 40.0], 1.0),
        ChildOf(item),
    ));
    grab_and_move(&mut app, (20.0, 20.0), (80.0, 80.0));
    assert!(active(&app));
    assert_eq!(app.world().resource::<DragDrop>().active().unwrap().hover, None);
}

#[test]
fn a_node_without_drag_source_never_starts_an_operation() {
    let mut app = app();
    node(&mut app, [0.0, 0.0, 40.0, 40.0]);
    target(&mut app, [200.0, 0.0, 80.0, 80.0], &[]);
    let events = grab_and_move(&mut app, (20.0, 20.0), (240.0, 40.0));
    assert!(events.is_empty());
    assert!(!active(&app));
}

#[test]
fn a_disabled_source_does_not_start() {
    let mut app = app();
    let item = source(&mut app, [0.0, 0.0, 40.0, 40.0], "item", 1);
    app.world_mut().entity_mut(item).insert(Disabled);
    grab_and_move(&mut app, (20.0, 20.0), (240.0, 40.0));
    assert!(!active(&app));
}

#[test]
fn despawning_the_source_cancels_with_source_gone() {
    let mut app = app();
    let item = source(&mut app, [0.0, 0.0, 40.0, 40.0], "item", 1);
    let slot = target(&mut app, [200.0, 0.0, 80.0, 80.0], &["item"]);
    grab_and_move(&mut app, (20.0, 20.0), (240.0, 40.0));
    app.world_mut().despawn(item);
    app.update();
    let events = drain(&mut app);
    assert!(events.contains(&DragDropEvent::Cancelled {
        source: item,
        reason: DragCancelReason::SourceGone,
    }));
    assert!(!active(&app));
    assert!(app.world().get::<DropHover>(slot).is_none());
}

#[test]
fn cancel_drag_requests_a_cancel_and_reports_whether_one_ran() {
    let mut app = app();
    assert!(!cancel_drag(&mut app.world_mut().resource_mut::<DragDrop>()));
    let item = source(&mut app, [0.0, 0.0, 40.0, 40.0], "item", 1);
    target(&mut app, [200.0, 0.0, 80.0, 80.0], &["item"]);
    grab_and_move(&mut app, (20.0, 20.0), (240.0, 40.0));
    assert!(cancel_drag(&mut app.world_mut().resource_mut::<DragDrop>()));
    app.update();
    let events = drain(&mut app);
    assert!(events.contains(&DragDropEvent::Cancelled {
        source: item,
        reason: DragCancelReason::Requested,
    }));
    assert!(!active(&app));
}

#[test]
fn a_layer_that_hides_the_source_cancels_and_targets_outside_the_layer_are_not_hit() {
    let mut app = app();
    let item = source(&mut app, [0.0, 0.0, 40.0, 40.0], "item", 1);
    let slot = target(&mut app, [200.0, 0.0, 80.0, 80.0], &["item"]);
    let modal = node(&mut app, [150.0, 0.0, 300.0, 300.0]);
    grab_and_move(&mut app, (20.0, 20.0), (240.0, 40.0));
    assert!(active(&app));
    push_layer(app.world_mut(), modal, LayerInputMode::UiOnly).unwrap();
    app.update();
    let events = drain(&mut app);
    assert!(events.contains(&DragDropEvent::Cancelled {
        source: item,
        reason: DragCancelReason::LayerChanged,
    }));
    assert!(app.world().get::<DropHover>(slot).is_none());
    pop_layer(app.world_mut(), None).unwrap();
}

#[test]
fn a_second_drag_works_after_a_cancel() {
    let mut app = app();
    let item = source(&mut app, [0.0, 0.0, 40.0, 40.0], "item", 1);
    let slot = target(&mut app, [200.0, 0.0, 80.0, 80.0], &["item"]);
    grab_and_move(&mut app, (20.0, 20.0), (240.0, 40.0));
    key(&mut app, "Escape");
    left(&mut app, ButtonState::Released);
    grab_and_move(&mut app, (20.0, 20.0), (240.0, 40.0));
    let events = left(&mut app, ButtonState::Released);
    assert!(events.contains(&DragDropEvent::Dropped {
        source: item,
        target: slot,
        payload: DragPayload::new("item", UiValue::Index(1)),
    }));
}
