// SPDX-License-Identifier: MPL-2.0
//! Runtime UI and input wiring (lane post05-ui-input, ADR-7070): spec 18 § 4 event bubbling, the gameplay
//! input gate behind `UiPointer.consumed_pointer`, UI navigation from the `Menu` input context, and the
//! spec 11 § 7 `InputGlyph` table. Headless: no window, no GPU, fixed steps, no goldens.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use regia_app::TimeUpdateStrategy;
use regia_app::prelude::*;
use regia_input::prelude::*;
use regia_platform::{ButtonState, MouseButton};
use regia_ui::focus::{DefaultFocus, FocusState, Focusable};
use regia_ui::hints::HintDevice;
use regia_ui::input_glyphs::{GlyphSource, InputGlyphTable, InputGlyphs};
use regia_ui::prelude::*;
use regia_ui::{UiBubbledEvent, UiStopBubble};

fn ui_app() -> App {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
    app.add_plugins(UiPlugin::code_only());
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::CursorMoved>()
        .add_message::<regia_platform::MouseButtonInput>()
        .add_message::<regia_platform::TouchInput>()
        .add_message::<regia_platform::MouseWheel>()
        .add_message::<regia_platform::KeyboardInput>()
        .add_message::<regia_platform::ImeEvent>();
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

fn move_to(app: &mut App, x: f64, y: f64) {
    app.world_mut().write_message(regia_platform::CursorMoved {
        window: Entity::PLACEHOLDER,
        position: (x, y),
        timestamp: Default::default(),
    });
    app.update();
}

fn mouse(app: &mut App, state: ButtonState) {
    app.world_mut().write_message(regia_platform::MouseButtonInput {
        window: Entity::PLACEHOLDER,
        button: MouseButton::Left,
        state,
        timestamp: Default::default(),
    });
    app.update();
}

fn key(app: &mut App, name: &str) {
    for state in [ButtonState::Pressed, ButtonState::Released] {
        app.world_mut().write_message(regia_platform::KeyboardInput {
            window: Entity::PLACEHOLDER,
            logical_key: name.into(),
            physical_key: name.into(),
            state,
            repeat: false,
            timestamp: Default::default(),
        });
        app.update();
    }
}

/// Every bubbled copy seen since the last call (both message buffers, so nothing written in an earlier
/// system of the frame is missed).
fn bubbled(
    app: &mut App,
    cursor: &mut regia_ecs::ecs::message::MessageCursor<UiBubbledEvent>,
) -> Vec<UiBubbledEvent> {
    let messages = app.world().resource::<Messages<UiBubbledEvent>>();
    cursor.read(messages).cloned().collect()
}

/// Spec 18 § 4: a click on a button reaches its ancestors as a bubbled copy (handler = the ancestor,
/// `event.target` = the button), nearest first; `UiStopBubble` on a node stops the walk above it; hover
/// events do not bubble.
#[test]
fn a_click_bubbles_to_the_ancestor_handler() {
    let mut app = ui_app();
    let root = app.world_mut().spawn(UiStopBubble).id();
    let panel = app.world_mut().spawn(ChildOf(root)).id();
    let row = app.world_mut().spawn(ChildOf(panel)).id();
    let button = app
        .world_mut()
        .spawn((
            Interactable::default(),
            computed([0.0, 0.0, 100.0, 40.0]),
            ChildOf(row),
        ))
        .id();
    app.update();
    let mut cursor = app.world().resource::<Messages<UiBubbledEvent>>().get_cursor();

    move_to(&mut app, 50.0, 20.0);
    let hover = bubbled(&mut app, &mut cursor);
    assert!(
        hover.iter().all(|e| e.event.kind != UiEventKind::PointerEnter),
        "hover does not bubble: {hover:?}"
    );
    mouse(&mut app, ButtonState::Pressed);
    mouse(&mut app, ButtonState::Released);
    let clicks: Vec<(Entity, Entity, u32)> = bubbled(&mut app, &mut cursor)
        .into_iter()
        .filter(|e| e.event.kind == UiEventKind::Click)
        .map(|e| (e.current, e.event.target, e.depth))
        .collect();
    assert_eq!(
        clicks,
        vec![(row, button, 1), (panel, button, 2), (root, button, 3)],
        "the click bubbles to every ancestor up to and including the stopping node"
    );

    // A stop on the row: the panel and the root no longer see the button's clicks.
    app.world_mut().entity_mut(row).insert(UiStopBubble);
    mouse(&mut app, ButtonState::Pressed);
    mouse(&mut app, ButtonState::Released);
    let clicks: Vec<Entity> = bubbled(&mut app, &mut cursor)
        .into_iter()
        .filter(|e| e.event.kind == UiEventKind::Click)
        .map(|e| e.current)
        .collect();
    assert_eq!(clicks, vec![row], "the walk stops at the row");
}

fn fire_app() -> App {
    let mut app = ui_app();
    app.add_plugins(InputPlugin::new(
        InputConfig::new()
            .action("Fire", ActionKind::Button)
            .binding(Binding::new("Fire", InputSource::mouse_button(MouseButton::Left))),
    ));
    app
}

fn fire_seen(app: &App) -> bool {
    let actions = app.world().resource::<Actions>();
    actions.pressed("Fire") || actions.just_pressed_this_frame("Fire")
}

/// Spec 18 § 4 input routing: a press over a button does not fire the gameplay "Fire" action (the UI
/// consumed the pointer), while the same press off the button does.
#[test]
fn a_press_over_a_button_does_not_fire_but_off_the_button_does() {
    let mut app = fire_app();
    app.world_mut()
        .spawn((Interactable::default(), computed([0.0, 0.0, 100.0, 40.0])));
    app.update();

    move_to(&mut app, 50.0, 20.0);
    mouse(&mut app, ButtonState::Pressed);
    let mut fired = fire_seen(&app);
    app.update();
    fired |= fire_seen(&app);
    assert!(!fired, "a press on the button must not also fire");
    mouse(&mut app, ButtonState::Released);
    assert!(!fire_seen(&app));

    move_to(&mut app, 400.0, 300.0);
    mouse(&mut app, ButtonState::Pressed);
    let mut fired = fire_seen(&app);
    app.update();
    fired |= fire_seen(&app);
    assert!(fired, "off the button the press fires");
    mouse(&mut app, ButtonState::Released);

    // The Menu context is exempt: a UI context still sees the pointer over its own widgets.
    assert!(app.world().resource::<UiPointerGate>().exempt(MENU_CONTEXT));
}

fn menu_app() -> (App, [Entity; 3]) {
    let mut app = ui_app();
    app.add_plugins(InputPlugin::new(InputConfig::new().with_menu_context()));
    app.world_mut()
        .resource_mut::<InputContexts>()
        .push(0, MENU_CONTEXT);
    let mut nodes = [Entity::PLACEHOLDER; 3];
    for (i, slot) in nodes.iter_mut().enumerate() {
        let y = 100.0 + 80.0 * i as f32;
        *slot = app
            .world_mut()
            .spawn((Focusable::default(), computed([100.0, y, 300.0, 60.0])))
            .id();
    }
    app.world_mut().entity_mut(nodes[2]).insert(DefaultFocus);
    app.update();
    (app, nodes)
}

fn focused(app: &App) -> Option<Entity> {
    app.world().resource::<FocusState>().focused
}

/// Spec 18 § 4: with the `Menu` input context enabled, UI navigation reads its actions, so rebinding the
/// context's "Up" changes what moves the focus up.
#[test]
fn rebinding_the_menu_up_changes_ui_navigation() {
    let (mut app, [a, b, c]) = menu_app();
    key(&mut app, "ArrowUp");
    assert_eq!(focused(&app), Some(c), "the first press focuses the default node");
    key(&mut app, "ArrowUp");
    assert_eq!(focused(&app), Some(b), "the default Up binding moves up");

    // Rebind the Menu context's Up from the arrow to W.
    {
        let mut config = app.world_mut().resource_mut::<InputConfig>();
        let menu = config.contexts.get_mut(MENU_CONTEXT).expect("menu context");
        for binding in &mut menu.bindings {
            if binding.action == MENU_UP && binding.input == InputSource::key("ArrowUp") {
                binding.input = InputSource::key("KeyW");
            }
        }
    }
    key(&mut app, "ArrowUp");
    assert_eq!(focused(&app), Some(b), "the old key no longer navigates");
    key(&mut app, "KeyW");
    assert_eq!(focused(&app), Some(a), "the rebound key navigates");
}

/// Spec 11 § 7: a glyph table asset overrides one action's glyph for one device family; the others fall
/// back to the built-in families (hint actions) or to the action's binding.
#[test]
fn a_glyph_asset_overrides_one_actions_glyph() {
    let table = InputGlyphTable::from_ron(
        r#"(version: 1, glyphs: { "activate": { "xbox": "Ⓐ" }, "Fire": { "keyboard_mouse": "LMB" } })"#,
    )
    .expect("table parses");
    let glyphs = InputGlyphs::new(table);
    let config = InputConfig::new()
        .action("Fire", ActionKind::Button)
        .action("Jump", ActionKind::Button)
        .binding(Binding::new("Fire", InputSource::mouse_button(MouseButton::Left)))
        .binding(Binding::new(
            "Fire",
            InputSource::gamepad_button(regia_platform::GamepadButton::RightTrigger),
        ))
        .binding(Binding::new("Jump", InputSource::key("Space")));

    let look = |action: &str, device: HintDevice| glyphs.lookup(action, device, Some(&config));
    let hit = look("activate", HintDevice::Xbox).expect("activate on xbox");
    assert_eq!((hit.glyph.as_str(), hit.source), ("Ⓐ", GlyphSource::Table));
    let fallback = look("activate", HintDevice::PlayStation).expect("built-in");
    assert_eq!(
        (fallback.glyph.as_str(), fallback.source),
        ("Cross", GlyphSource::BuiltIn)
    );
    let fire = look("Fire", HintDevice::KeyboardMouse).expect("fire kbm");
    assert_eq!((fire.glyph.as_str(), fire.source), ("LMB", GlyphSource::Table));
    let fire_pad = look("Fire", HintDevice::Xbox).expect("fire pad from binding");
    assert_eq!(
        (fire_pad.glyph.as_str(), fire_pad.source),
        ("RT", GlyphSource::Binding)
    );
    let jump = look("Jump", HintDevice::KeyboardMouse).expect("jump from binding");
    assert_eq!(
        (jump.glyph.as_str(), jump.source),
        ("Space", GlyphSource::Binding)
    );
    assert!(
        look("Jump", HintDevice::Xbox).is_none(),
        "no pad binding, no glyph"
    );
    assert!(
        InputGlyphTable::from_ron("(version: 99, glyphs: {})").is_err(),
        "a newer table version is refused"
    );
}
