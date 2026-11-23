// SPDX-License-Identifier: MPL-2.0
//! Screen/page stack (gap W18, ADR-273): push/pop history, visibility, input-mode and
//! focus restore, jumping back with `show_screen`, pruning of despawned screens, and errors.
//! Headless: no window, no GPU, game time advances by a fixed 100 ms per update.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use regia_app::TimeUpdateStrategy;
use regia_app::prelude::*;
use regia_ui::focus::{DefaultFocus, FocusState, Focusable};
use regia_ui::layers::{InputLayer, InputLayers, LayerInputMode};
use regia_ui::prelude::*;
use regia_ui::screens::{
    ScreenChanged, ScreenError, ScreenStack, pop_screen, push_screen, screen_list, show_screen,
};
use regia_ui::style::{Dimension, FlexDirection, Style};

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
    app.init_resource::<Seen>().add_systems(PostUpdate, record_events);
    app
}

/// Every `ScreenChanged` of the run, each read once.
#[derive(Resource, Default)]
struct Seen(Vec<ScreenChanged>);

fn record_events(mut events: MessageReader<ScreenChanged>, mut seen: ResMut<Seen>) {
    seen.0.extend(events.read().cloned());
}

/// A full-screen page with one default-focus button. Returns `(screen, button)`.
fn screen(app: &mut App, id: &str, mode: LayerInputMode) -> (Entity, Entity) {
    let root = app
        .world_mut()
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                direction: FlexDirection::Column,
                size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                ..Default::default()
            }),
            UiScreen::new(id).with_mode(mode),
        ))
        .id();
    let button = app
        .world_mut()
        .spawn((UiNode, Focusable::default(), DefaultFocus, ChildOf(root)))
        .id();
    (root, button)
}

fn display(app: &App, e: Entity) -> Display {
    app.world()
        .get::<NodeStyle>(e)
        .expect("screen has a style")
        .0
        .display
}

fn focused(app: &App) -> Option<Entity> {
    app.world().resource::<FocusState>().focused
}

fn mode(app: &App) -> LayerInputMode {
    app.world().resource::<InputLayers>().mode()
}

fn depth(app: &App) -> usize {
    app.world().resource::<ScreenStack>().depth()
}

#[test]
fn push_shows_the_top_and_hides_the_previous() {
    let mut app = app();
    let (a, _) = screen(&mut app, "title", LayerInputMode::UiOnly);
    let (b, button_b) = screen(&mut app, "options", LayerInputMode::UiOnly);
    push_screen(app.world_mut(), a).expect("push title");
    push_screen(app.world_mut(), b).expect("push options");
    app.update();

    assert_eq!(display(&app, a), Display::None, "buried screen hidden");
    assert_ne!(display(&app, b), Display::None, "top screen shown");
    assert_eq!(depth(&app), 2);
    assert_eq!(app.world().resource::<ScreenStack>().top(), Some(b));
    assert_eq!(focused(&app), Some(button_b), "top default focused");
    assert!(
        app.world()
            .resource::<Seen>()
            .0
            .iter()
            .any(|e| e.top == Some(b) && e.pushed == Some(b)),
        "push emits ScreenChanged with the new top"
    );
}

#[test]
fn pop_restores_the_control_that_had_focus() {
    let mut app = app();
    let (a, play) = screen(&mut app, "title", LayerInputMode::UiOnly);
    let quit = app
        .world_mut()
        .spawn((UiNode, Focusable::default(), ChildOf(a)))
        .id();
    let (b, _) = screen(&mut app, "options", LayerInputMode::UiOnly);
    push_screen(app.world_mut(), a).expect("push title");
    app.update();
    assert_eq!(focused(&app), Some(play));
    app.world_mut().resource_mut::<FocusState>().focused = Some(quit);
    push_screen(app.world_mut(), b).expect("push options");
    app.update();
    pop_screen(app.world_mut(), None).expect("pop");
    app.update();
    assert_eq!(
        focused(&app),
        Some(quit),
        "pop restores the control that had focus"
    );
}

#[test]
fn pop_restores_visibility_focus_and_input_mode() {
    let mut app = app();
    let (a, button_a) = screen(&mut app, "title", LayerInputMode::UiOnly);
    let (b, _) = screen(&mut app, "hud", LayerInputMode::GameOnly);
    push_screen(app.world_mut(), a).expect("push title");
    app.update();
    push_screen(app.world_mut(), b).expect("push hud");
    app.update();
    assert_eq!(mode(&app), LayerInputMode::GameOnly);

    let popped = pop_screen(app.world_mut(), None).expect("pop hud");
    assert_eq!(popped, b);
    app.update();

    assert_eq!(display(&app, b), Display::None, "popped screen hidden");
    assert_ne!(display(&app, a), Display::None, "previous screen shown again");
    assert_eq!(
        focused(&app),
        Some(button_a),
        "focus returns to the previous screen"
    );
    assert_eq!(mode(&app), LayerInputMode::UiOnly, "input mode restored");
    assert!(app.world().get::<InputLayer>(b).is_none(), "popped layer removed");
    assert!(
        app.world().get::<InputLayer>(a).is_some(),
        "restored layer present"
    );
}

#[test]
fn show_screen_jumps_back_over_history() {
    let mut app = app();
    let (a, button_a) = screen(&mut app, "title", LayerInputMode::UiOnly);
    let (b, _) = screen(&mut app, "options", LayerInputMode::UiOnly);
    let (c, _) = screen(&mut app, "credits", LayerInputMode::UiOnly);
    for s in [a, b, c] {
        push_screen(app.world_mut(), s).expect("push");
    }
    app.update();

    assert!(show_screen(app.world_mut(), a).expect("jump back"));
    app.update();
    assert_eq!(app.world().resource::<ScreenStack>().top(), Some(a));
    assert_eq!(depth(&app), 1, "screens above are popped");
    assert_ne!(display(&app, a), Display::None);
    assert_eq!(display(&app, b), Display::None);
    assert_eq!(display(&app, c), Display::None);
    assert_eq!(focused(&app), Some(button_a));
    assert!(
        !show_screen(app.world_mut(), a).expect("already on top"),
        "no-op reports false"
    );

    let list = screen_list(app.world());
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, "title");
    assert!(list[0].visible);
}

#[test]
fn despawned_top_prunes_and_focus_recovers() {
    let mut app = app();
    let (a, button_a) = screen(&mut app, "title", LayerInputMode::UiOnly);
    let (b, _) = screen(&mut app, "options", LayerInputMode::UiOnly);
    push_screen(app.world_mut(), a).expect("push title");
    push_screen(app.world_mut(), b).expect("push options");
    app.update();

    app.world_mut().entity_mut(b).despawn();
    app.update();

    assert_eq!(app.world().resource::<ScreenStack>().top(), Some(a));
    assert_ne!(display(&app, a), Display::None, "previous screen shown again");
    assert_eq!(focused(&app), Some(button_a), "focus recovers instead of locking");
}

#[test]
fn screen_errors_use_stable_codes() {
    let mut app = app();
    let (a, _) = screen(&mut app, "title", LayerInputMode::UiOnly);
    let (b, _) = screen(&mut app, "options", LayerInputMode::UiOnly);
    let plain = app.world_mut().spawn(UiNode).id();

    assert_eq!(pop_screen(app.world_mut(), None), Err(ScreenError::EmptyStack));
    assert_eq!(push_screen(app.world_mut(), plain), Err(ScreenError::NotAScreen));
    assert_eq!(show_screen(app.world_mut(), plain), Err(ScreenError::NotAScreen));
    push_screen(app.world_mut(), a).expect("push title");
    assert_eq!(push_screen(app.world_mut(), a), Err(ScreenError::AlreadyOnStack));
    push_screen(app.world_mut(), b).expect("push options");
    assert_eq!(pop_screen(app.world_mut(), Some(a)), Err(ScreenError::NotOnTop));
    let ghost = app.world_mut().spawn(UiNode).id();
    app.world_mut().entity_mut(ghost).despawn();
    assert_eq!(
        push_screen(app.world_mut(), ghost),
        Err(ScreenError::UnknownEntity)
    );
}

#[test]
fn back_opt_in_pops_once_and_restores_focus() {
    let mut app = app();
    let (a, a_button) = screen(&mut app, "main", LayerInputMode::UiOnly);
    let (b, _) = screen(&mut app, "settings", LayerInputMode::UiOnly);
    push_screen(app.world_mut(), a).unwrap();
    app.update();
    push_screen(app.world_mut(), b).unwrap();
    app.update();
    app.world_mut()
        .resource_mut::<regia_ui::focus::UiNavActions>()
        .cancel = true;
    app.update();
    assert_eq!(depth(&app), 2, "back is opt-in");
    app.world_mut().resource_mut::<ScreenStack>().set_back_pop(true);
    app.world_mut()
        .resource_mut::<regia_ui::focus::UiNavActions>()
        .cancel = true;
    app.update();
    assert_eq!(depth(&app), 1);
    assert_eq!(focused(&app), Some(a_button));
    app.update();
    assert_eq!(depth(&app), 1, "one press pops once");
}

#[test]
fn recursive_disable_tracks_hierarchy_and_preserves_local_disable() {
    let mut app = app();
    let (a, button) = screen(&mut app, "main", LayerInputMode::UiOnly);
    let local = app.world_mut().spawn((UiNode, Disabled, ChildOf(a))).id();
    push_screen(app.world_mut(), a).unwrap();
    app.update();
    app.world_mut().entity_mut(a).insert(Disabled::recursive());
    app.update();
    assert!(regia_ui::node::is_disabled(app.world(), button));
    assert_ne!(focused(&app), Some(button));
    let later = app.world_mut().spawn((UiNode, ChildOf(a))).id();
    assert!(regia_ui::node::is_disabled(app.world(), later));
    app.world_mut()
        .entity_mut(a)
        .remove::<regia_ui::node::DisabledSubtree>();
    app.update();
    assert!(!regia_ui::node::is_disabled(app.world(), button));
    assert!(regia_ui::node::is_disabled(app.world(), local));
}

#[test]
fn escape_and_gamepad_back_respect_screen_override_and_modal_layer() {
    use regia_platform::{ButtonState, GamepadButton, GamepadButtonChanged, KeyboardInput};
    use regia_ui::screens::ScreenBack;
    let mut app = app();
    let (a, button) = screen(&mut app, "main", LayerInputMode::UiOnly);
    let (b, _) = screen(&mut app, "options", LayerInputMode::UiOnly);
    push_screen(app.world_mut(), a).unwrap();
    app.update();
    push_screen(app.world_mut(), b).unwrap();
    app.world_mut().entity_mut(b).insert(ScreenBack::Pop);
    app.update();
    let escape = |app: &mut App| {
        for state in [ButtonState::Pressed, ButtonState::Released] {
            app.world_mut().write_message(KeyboardInput {
                window: Entity::PLACEHOLDER,
                physical_key: "Escape".into(),
                logical_key: "Escape".into(),
                state,
                repeat: false,
                timestamp: Default::default(),
            });
            app.update();
        }
    };
    escape(&mut app);
    assert_eq!(depth(&app), 1);
    assert_eq!(focused(&app), Some(button));
    push_screen(app.world_mut(), b).unwrap();
    app.update();
    let modal = app.world_mut().spawn(UiNode).id();
    regia_ui::layers::push_layer(app.world_mut(), modal, LayerInputMode::UiOnly).unwrap();
    escape(&mut app);
    assert_eq!(depth(&app), 2, "an overlaid layer owns Escape");
    regia_ui::layers::pop_layer(app.world_mut(), Some(modal)).unwrap();
    app.update();
    let mut capture = regia_input::InputRebinder::default();
    capture.start("Menu", "Cancel");
    app.world_mut().insert_resource(capture);
    escape(&mut app);
    assert_eq!(depth(&app), 2, "rebinding owns the back press");
    app.world_mut().remove_resource::<regia_input::InputRebinder>();
    app.world_mut().entity_mut(b).insert(ScreenBack::Ignore);
    app.world_mut().resource_mut::<ScreenStack>().set_back_pop(true);
    escape(&mut app);
    assert_eq!(depth(&app), 2, "per-screen ignore overrides stack opt-in");
    app.world_mut().entity_mut(b).insert(ScreenBack::Pop);
    for state in [ButtonState::Pressed, ButtonState::Released] {
        app.world_mut().write_message(GamepadButtonChanged {
            device: 7,
            button: GamepadButton::East,
            state,
            value: 1.0,
        });
        app.update();
    }
    assert_eq!(depth(&app), 1);
    assert_eq!(focused(&app), Some(button));
}

#[test]
fn disabling_a_pressed_subtree_cancels_pointer_input_and_reparenting_restores_it() {
    use regia_ui::interaction::UiPointer;
    use regia_ui::node::{ComputedNode, Pressed};
    let mut app = app();
    let parent = app.world_mut().spawn(UiNode).id();
    let other = app.world_mut().spawn(UiNode).id();
    let child = app
        .world_mut()
        .spawn((
            UiNode,
            ChildOf(parent),
            Interactable::default(),
            ComputedNode {
                rect: [0.0, 0.0, 100.0, 100.0],
                clip_rect: [0.0, 0.0, 100.0, 100.0],
                ..Default::default()
            },
        ))
        .id();
    app.update();
    {
        let mut pointer = app.world_mut().resource_mut::<UiPointer>();
        pointer.cursor = Some([20.0, 20.0]);
        pointer.primary_down = true;
    }
    app.update();
    assert_eq!(app.world().resource::<UiPointer>().pressed, Some(child));
    app.world_mut().entity_mut(parent).insert(Disabled::recursive());
    app.update();
    let pointer = app.world().resource::<UiPointer>();
    assert_eq!(pointer.pressed, None);
    assert_eq!(pointer.hovered, None);
    assert!(app.world().get::<Pressed>(child).is_none());
    app.world_mut().entity_mut(child).insert(ChildOf(other));
    assert!(!regia_ui::node::is_disabled(app.world(), child));
    app.world_mut().resource_mut::<UiPointer>().primary_down = false;
    app.update();
    app.world_mut().resource_mut::<UiPointer>().primary_down = true;
    app.update();
    assert_eq!(app.world().resource::<UiPointer>().pressed, Some(child));
}

#[test]
fn recursive_disabled_state_reaches_accessibility_and_restored_focus_skips_it() {
    let mut app = app();
    let ui_root = app.world_mut().spawn(UiRoot::default()).id();
    let (a, previous) = screen(&mut app, "main", LayerInputMode::UiOnly);
    app.world_mut().entity_mut(a).insert(ChildOf(ui_root));
    let (b, _) = screen(&mut app, "options", LayerInputMode::UiOnly);
    app.world_mut().entity_mut(b).insert(ChildOf(ui_root));
    push_screen(app.world_mut(), a).unwrap();
    app.update();
    push_screen(app.world_mut(), b).unwrap();
    app.update();
    app.world_mut().entity_mut(a).insert(Disabled::recursive());
    pop_screen(app.world_mut(), None).unwrap();
    app.update();
    assert_ne!(focused(&app), Some(previous));
    let tree = regia_ui::a11y::build_a11y_tree(app.world_mut());
    assert!(
        tree.nodes
            .iter()
            .find(|node| node.id == previous.to_bits())
            .unwrap()
            .states
            .disabled
    );
}
