// SPDX-License-Identifier: MPL-2.0
//! Composed focus histories use the same CPU update path as keyboard and gamepad navigation.
//! The mutation phases run on standalone nodes (no `UiRoot`, like `focus_graph.rs`, so the
//! layout pass never rewrites the hand-set computed rects); the accessibility phase builds a
//! real styled screen because the a11y tree roots at `UiRoot` entities.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use regia_app::TimeUpdateStrategy;
use regia_app::prelude::*;
use regia_platform::{ButtonState, GamepadButton, GamepadButtonChanged};
use regia_ui::a11y::{A11yAction, A11yRequest, apply_request};
use regia_ui::focus::{DefaultFocus, FocusScope, FocusState, Focusable, NavDirection, UiInputMode};
use regia_ui::focus_graph::{nav_move, world_defaults, world_graph, world_origin};
use regia_ui::prelude::*;
use regia_ui::{Disabled, Focused, UiEvent, UiEventKind};

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

/// A standalone focusable node with a hand-set computed rect. With no `UiRoot` in the world the
/// layout pass leaves it alone, so mutations of the rect persist across updates.
fn rect(x: f32) -> ComputedNode {
    ComputedNode {
        rect: [x, 0.0, 90.0, 40.0],
        clip_rect: [x, 0.0, 90.0, 40.0],
        scale: 1.0,
        ..Default::default()
    }
}

fn keyboard(app: &mut App, key: &str) {
    app.world_mut().write_message(regia_platform::KeyboardInput {
        window: Entity::PLACEHOLDER,
        physical_key: key.into(),
        logical_key: key.into(),
        state: ButtonState::Pressed,
        repeat: false,
        timestamp: Default::default(),
    });
    app.update();
    app.world_mut().write_message(regia_platform::KeyboardInput {
        window: Entity::PLACEHOLDER,
        physical_key: key.into(),
        logical_key: key.into(),
        state: ButtonState::Released,
        repeat: false,
        timestamp: Default::default(),
    });
    app.update();
}

fn gamepad(app: &mut App, button: GamepadButton) {
    for state in [ButtonState::Pressed, ButtonState::Released] {
        app.world_mut().write_message(GamepadButtonChanged {
            device: 7,
            button,
            state,
            value: 1.0,
        });
        app.update();
    }
}

fn current(app: &App) -> Option<Entity> {
    app.world().resource::<FocusState>().focused
}

fn gained(app: &mut App) -> usize {
    app.world_mut()
        .resource_mut::<Messages<UiEvent>>()
        .iter_current_update_messages()
        .filter(|event| event.kind == UiEventKind::FocusChanged(true))
        .count()
}

/// The invariant every mutation history step checks: the current focus names a live, eligible
/// node (or the documented empty state), never a hidden, disabled or despawned one.
fn assert_current_is_eligible(app: &App, eligible: &[Entity]) {
    let focus = current(app);
    assert!(
        focus.is_none_or(|entity| {
            eligible.contains(&entity)
                && app.world().get_entity(entity).is_ok()
                && app.world().get::<Disabled>(entity).is_none()
                && app.world().get::<Focusable>(entity).is_some()
                && app
                    .world()
                    .get::<ComputedNode>(entity)
                    .is_some_and(|node| node.rect[2] > 0.0 && node.rect[3] > 0.0)
        }),
        "focus={focus:?}, eligible={eligible:?}"
    );
    if let Some(entity) = focus {
        assert!(app.world().get::<Focused>(entity).is_some());
    }
}

#[test]
fn mutation_history_keeps_focus_live_and_matches_accessibility_navigation() {
    let mut app = app();
    *app.world_mut().resource_mut::<UiInputMode>() = UiInputMode::Navigation;
    let branch = app.world_mut().spawn_empty().id();
    let a = app
        .world_mut()
        .spawn((Focusable::default(), DefaultFocus, rect(0.0), ChildOf(branch)))
        .id();
    let b = app
        .world_mut()
        .spawn((Focusable::default(), rect(120.0), ChildOf(branch)))
        .id();
    let c = app
        .world_mut()
        .spawn((Focusable::default(), rect(240.0), ChildOf(branch)))
        .id();
    let d = app.world_mut().spawn((Focusable::default(), rect(360.0))).id();
    app.update();

    // Initial focus and ordinary keyboard traversal establish the history.
    keyboard(&mut app, "Tab");
    assert_eq!(current(&app), Some(a));
    keyboard(&mut app, "Tab");
    assert_eq!(current(&app), Some(b));
    assert_eq!(gained(&mut app), 0, "released frame has no duplicate focus event");

    // Hiding the focused node strands it: the graph reads each node's own computed rect, so the
    // zeroed node (and only it) leaves navigation.
    app.world_mut().get_mut::<ComputedNode>(b).unwrap().rect = [120.0, 0.0, 0.0, 0.0];
    app.update();
    assert_current_is_eligible(&app, &[a, c, d]);

    // Reparenting a surviving child preserves its identity and eligibility.
    app.world_mut().entity_mut(c).insert(ChildOf(d));
    app.update();
    assert!(app.world().get_entity(c).is_ok());
    assert_current_is_eligible(&app, &[a, c, d]);

    // Disabling the focused node itself strands it; focus repairs to a live sibling.
    let focused_before = current(&app).expect("focus before disable");
    app.world_mut().entity_mut(focused_before).insert(Disabled);
    app.update();
    assert_current_is_eligible(&app, &[a, c, d]);
    let focused_after = current(&app).expect("focus after disable");
    assert_ne!(focused_after, focused_before, "repair leaves the disabled node");
    assert!(
        app.world().get::<Disabled>(focused_after).is_none(),
        "repair never leaves focus on a disabled node"
    );

    // Disabling a non-focusable ancestor follows the documented contract: only the disabled
    // node itself leaves navigation, so its children stay eligible and focus may stay.
    app.world_mut().entity_mut(branch).insert(Disabled);
    app.update();
    assert_current_is_eligible(&app, &[a, c, d]);
    app.world_mut().entity_mut(branch).remove::<Disabled>();
    app.update();
    assert_current_is_eligible(&app, &[a, c, d]);

    // Removing an ancestor removes its descendants; focus repairs to a surviving node.
    app.world_mut().despawn(branch);
    app.update();
    assert!(app.world().get_entity(a).is_err());
    assert!(app.world().get_entity(b).is_err());
    assert_current_is_eligible(&app, &[c, d]);
    let moved = nav_move(app.world_mut(), NavDirection::Right).unwrap();
    assert!(moved.focused.is_some());
    assert_current_is_eligible(&app, &[c, d]);

    // A focused node removed during navigation is repaired in the same update.
    if let Some(focus) = current(&app) {
        app.world_mut().despawn(focus);
    }
    app.update();
    assert_current_is_eligible(&app, &[c, d]);

    // Keyboard and gamepad moves share the same graph after the mutations.
    gamepad(&mut app, GamepadButton::DPadRight);
    assert_current_is_eligible(&app, &[c, d]);
    keyboard(&mut app, "Tab");
    assert_current_is_eligible(&app, &[c, d]);
    keyboard(&mut app, "ShiftLeft");
    keyboard(&mut app, "Tab");
    assert_current_is_eligible(&app, &[c, d]);

    // Accessibility focus drives the same FocusState/Focused markers as the keyboard. The
    // request names a node on a real styled screen (the a11y tree roots at `UiRoot`), and the
    // next keyboard move agrees with a direct graph resolve.
    let screen = app.world_mut().spawn(UiRoot::default()).id();
    let target = styled_button(&mut app, screen, 0.0);
    let other = styled_button(&mut app, screen, 120.0);
    app.update();
    assert_current_is_eligible(&app, &[c, d, target, other]);
    assert_eq!(
        apply_request(
            app.world_mut(),
            &A11yRequest {
                node: target.to_bits(),
                action: A11yAction::Focus
            }
        ),
        Ok(())
    );
    assert_eq!(current(&app), Some(target));
    assert!(app.world().get::<Focused>(target).is_some());
    let graph = world_graph(app.world_mut());
    let defaults = world_defaults(app.world_mut());
    let memory = app
        .world()
        .resource::<regia_ui::focus_graph::FocusMemory>()
        .clone();
    let expected = graph
        .resolve(
            world_origin(app.world(), &graph),
            NavDirection::Next,
            &defaults,
            &memory,
        )
        .target;
    keyboard(&mut app, "Tab");
    assert_eq!(current(&app), expected);
    assert_current_is_eligible(&app, &[c, d, target, other]);
}

/// A focusable button laid out by the real layout pass (fixed size, absolutely positioned), so
/// its computed rect survives relayouts under the `UiRoot` the accessibility tree walks.
fn styled_button(app: &mut App, parent: Entity, left: f32) -> Entity {
    app.world_mut()
        .spawn((
            UiNode,
            Focusable::default(),
            NodeStyle::new(Style {
                position: PositionType::Absolute,
                inset: Edges {
                    left: Dimension::Points(left),
                    ..Default::default()
                },
                size: [Dimension::Points(90.0), Dimension::Points(40.0)],
                ..Default::default()
            }),
            ChildOf(parent),
        ))
        .id()
}

#[test]
fn recycled_entity_index_does_not_resurrect_old_focus_generation() {
    let mut app = app();
    *app.world_mut().resource_mut::<UiInputMode>() = UiInputMode::Navigation;
    let old = app
        .world_mut()
        .spawn((Focusable::default(), DefaultFocus, rect(0.0)))
        .id();
    app.update();
    assert_eq!(
        nav_move(app.world_mut(), NavDirection::Next).unwrap().focused,
        Some(old)
    );
    app.world_mut().despawn(old);

    // Recreate a focusable at the same slot. The allocator defers frees to a 128-entry local
    // list and recycles last-freed-first once that list flushes to the shared recycler, so:
    // free a burst larger than the list (the old node is among the first flushed entries), then
    // spawn while keeping every spawn alive — the walk down the recycled slots cannot cycle
    // back over a slot it never freed again. The slot comes back with a bumped generation.
    let pending: Vec<Entity> = app.world_mut().spawn_batch((0..300).map(|_| ())).collect();
    for entity in pending {
        app.world_mut().despawn(entity);
    }
    let mut replacement = None;
    for _ in 0..300 {
        let candidate = app.world_mut().spawn_empty().id();
        if candidate.index() == old.index() {
            replacement = Some(candidate);
            break;
        }
    }
    let replacement = replacement.expect("the recycled slot surfaces within the flush batches");
    assert_ne!(old, replacement, "the recycled slot has a new generation");
    app.world_mut()
        .entity_mut(replacement)
        .insert((Focusable::default(), DefaultFocus, rect(0.0)));

    // The update after the mutation must not name the dead entity; repair either clears focus
    // or lands on the live replacement, never on the stale bits.
    app.update();
    assert_ne!(current(&app), Some(old), "a dead entity is never named as focus");
    assert_current_is_eligible(&app, &[replacement]);
    assert_eq!(
        nav_move(app.world_mut(), NavDirection::Next).unwrap().focused,
        Some(replacement)
    );
    assert!(app.world().get::<Focused>(replacement).is_some());
}

#[test]
fn emptied_and_restored_modal_group_never_leaks_focus() {
    let mut app = app();
    let outside = app
        .world_mut()
        .spawn((Focusable::default(), DefaultFocus, rect(0.0)))
        .id();
    let modal = app
        .world_mut()
        .spawn((
            FocusScope,
            Modal {
                open: false,
                ..Default::default()
            },
        ))
        .id();
    let dialog = app
        .world_mut()
        .spawn((Focusable::default(), DefaultFocus, rect(160.0), ChildOf(modal)))
        .id();
    app.update();
    gamepad(&mut app, GamepadButton::DPadRight);
    assert_eq!(current(&app), Some(outside));
    app.world_mut().get_mut::<Modal>(modal).unwrap().open = true;
    app.update();
    assert_eq!(
        current(&app),
        Some(dialog),
        "opening the modal takes its default focus"
    );
    app.world_mut().despawn(dialog);
    app.update();
    assert_eq!(
        current(&app),
        None,
        "an empty modal has the documented empty focus state"
    );
    assert_ne!(current(&app), Some(outside));
    let restored = app
        .world_mut()
        .spawn((Focusable::default(), DefaultFocus, rect(180.0), ChildOf(modal)))
        .id();
    let second = app
        .world_mut()
        .spawn((Focusable::default(), rect(300.0), ChildOf(modal)))
        .id();
    app.update();
    assert_eq!(
        current(&app),
        Some(restored),
        "a restored default child takes focus again"
    );
    assert_ne!(current(&app), Some(outside));

    // Forward/back navigation stays inside the active modal.
    keyboard(&mut app, "Tab");
    assert_eq!(
        current(&app),
        Some(second),
        "Tab moves to the modal's other child"
    );
    assert_ne!(current(&app), Some(outside));
    keyboard(&mut app, "ShiftLeft");
    keyboard(&mut app, "Tab");
    assert_eq!(current(&app), Some(restored), "Shift-Tab moves back");
    keyboard(&mut app, "ShiftLeft");
    gamepad(&mut app, GamepadButton::DPadRight);
    assert!(
        current(&app).is_some_and(|e| { app.world().get::<ChildOf>(e).is_some_and(|p| p.0 == modal) }),
        "a gamepad move never escapes the modal: {:?}",
        current(&app)
    );
}

#[test]
fn bounded_fixed_seed_mutations_preserve_the_independent_live_target_model() {
    let mut app = app();
    let mut nodes = Vec::new();
    for index in 0..8 {
        let entity = app
            .world_mut()
            .spawn((Focusable::default(), rect(index as f32 * 110.0)))
            .id();
        nodes.push((entity, true));
    }
    app.update();
    let mut seed = 0x4d59_5df4_d0f3_3173_u64;
    for step in 0..64 {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let index = ((seed >> 32) as usize) % nodes.len();
        let (entity, live) = &mut nodes[index];
        if *live {
            if seed & 1 == 0 {
                app.world_mut().get_mut::<ComputedNode>(*entity).unwrap().rect[2] = 0.0;
            } else {
                app.world_mut().despawn(*entity);
                *live = false;
            }
        } else {
            *entity = app
                .world_mut()
                .spawn((Focusable::default(), rect(index as f32 * 110.0)))
                .id();
            *live = true;
        }
        app.update();
        let eligible: Vec<Entity> = nodes
            .iter()
            .filter_map(|(entity, live)| {
                (*live
                    && app
                        .world()
                        .get::<ComputedNode>(*entity)
                        .is_some_and(|node| node.rect[2] > 0.0))
                .then_some(*entity)
            })
            .collect();
        assert_current_is_eligible(&app, &eligible);
        if step % 5 == 0 {
            let _ = nav_move(app.world_mut(), NavDirection::Next);
        }
    }
}
