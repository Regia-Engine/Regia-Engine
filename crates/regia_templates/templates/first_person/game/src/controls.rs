// SPDX-License-Identifier: MIT-0
//! A controls screen: F10 opens a frosted panel over the running game where the player rebinds each
//! action per device, tunes the stick and mouse axes (dead zone, invert), resets, and saves a named
//! profile. Everything is `regia_ui::RebindPanel`; the capture, the merge over `config/input` and
//! the save to `user://input_overrides.ron` are `regia_input`'s. Delete this file and its plugin
//! line in `lib.rs` when your game has its own menus.
//!
//! On a phone or tablet ([`ControlsLayout::Touch`], the Android default) there is no F10: an
//! on-screen **Controls** button opens the screen, and **Close** or the system Back closes it (Back
//! does not quit the game while the screen is open). The screen is laid out in device-independent
//! px, so every control keeps the 44 px touch size, and the bindings scroll inside the safe area.
//!
//! An agent drives it like a player: `play.inject_input` for F10 (or `ui.click` on `controls.open`),
//! `ui.click` on `rebind.Gameplay.Move.gamepad`, or the `input.rebind*` methods directly.

use regia::prelude::*;
use regia::ui::interaction::{UiEvent, UiEventKind};
use regia::ui::style::{Align, Dimension, FlexDirection};
use regia::ui::widgets::Button;
use regia::ui::{
    BackdropBlur, BackgroundKind, NodeName, RebindPanel, RebindPanelOptions, TouchTarget, UiDensity,
    scroll_container,
};
use regia_input::{InputRebinder, Keyboard};

/// The key that opens and closes the screen.
const TOGGLE_KEY: &str = "F10";

/// Whether the controls screen is open; gameplay reads it to ignore input meanwhile.
#[derive(Resource, Default)]
pub struct ControlsOpen(pub bool);

/// How the screen is sized and opened.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlsLayout {
    /// The project's reference resolution; F10 opens and closes.
    Desktop,
    /// A touch screen: 1 UI px is one device-independent px, the bindings scroll, an on-screen
    /// button opens the screen and Close or Back closes it.
    Touch,
}

impl Default for ControlsLayout {
    fn default() -> Self {
        if cfg!(target_os = "android") {
            Self::Touch
        } else {
            Self::Desktop
        }
    }
}

/// The root node of the open screen.
#[derive(Component)]
struct ControlsRoot;

/// The on-screen button that opens the screen (touch layout).
#[derive(Component)]
struct OpenButton;

/// The button that closes the screen (touch layout).
#[derive(Component)]
struct CloseButton;

/// Adds the controls screen.
pub struct ControlsPlugin;

impl Plugin for ControlsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ControlsOpen>()
            .init_resource::<ControlsLayout>()
            .init_resource::<ClosePolicy>()
            .add_message::<BackPressed>()
            .add_systems(Update, (open_button, toggle_controls).chain());
    }
}

/// `Update`: the touch layout's open button is on screen while the screen is shut (it would sit
/// on the screen's Close button otherwise); the desktop layout has none.
fn open_button(
    mut commands: Commands,
    layout: Res<ControlsLayout>,
    open: Res<ControlsOpen>,
    buttons: Query<&ChildOf, With<OpenButton>>,
) {
    let want = *layout == ControlsLayout::Touch && !open.0;
    if want && buttons.is_empty() {
        spawn_open_button(&mut commands);
    } else if !want {
        for parent in &buttons {
            commands.entity(parent.parent()).despawn();
        }
    }
}

/// What `toggle_controls` remembers between frames.
#[derive(Default)]
struct ToggleState {
    /// This screen turned `ClosePolicy::back_closes_primary` off and owes it back.
    held_back: bool,
    back: regia::ecs::ecs::message::MessageCursor<BackPressed>,
    events: regia::ecs::ecs::message::MessageCursor<UiEvent>,
}

/// `Update`: F10 or the open button opens the screen; F10, Escape, Close or Back closes it, unless
/// a capture is running (Escape and Back then leave the capture).
fn toggle_controls(world: &mut World, mut state: Local<ToggleState>) {
    let was_open = world.resource::<ControlsOpen>().0;
    // While the screen is open, Back closes it instead of the game. The policy is lifted in the
    // frame that opens the screen and given back one frame after it closes, so the Back that closed
    // it never reaches the window.
    let mut policy = world.resource_mut::<ClosePolicy>();
    if was_open && policy.back_closes_primary {
        policy.back_closes_primary = false;
        state.held_back = true;
    } else if !was_open && state.held_back {
        policy.back_closes_primary = true;
        state.held_back = false;
    }
    let state = &mut *state;
    let back = world
        .get_resource::<Messages<BackPressed>>()
        .is_some_and(|m| state.back.read(m).count() > 0);
    let clicked: Vec<Entity> = world
        .get_resource::<Messages<UiEvent>>()
        .map(|m| {
            state
                .events
                .read(m)
                .filter(|e| matches!(e.kind, UiEventKind::Click))
                .map(|e| e.target)
                .collect()
        })
        .unwrap_or_default();
    let open_clicked = clicked.iter().any(|e| world.get::<OpenButton>(*e).is_some());
    let close_clicked = clicked.iter().any(|e| world.get::<CloseButton>(*e).is_some());
    let keyboard = world.resource::<Keyboard>();
    let toggle = keyboard.just_pressed(TOGGLE_KEY);
    let close = toggle || keyboard.just_pressed("Escape") || back || close_clicked;
    let rebinder = world.resource::<InputRebinder>();
    let capturing = rebinder.pending().is_some() || rebinder.click_guard();
    if was_open {
        if close && !capturing {
            let roots: Vec<Entity> = world
                .query_filtered::<Entity, With<ControlsRoot>>()
                .iter(world)
                .collect();
            for root in roots {
                world.entity_mut(root).despawn();
            }
            world.resource_mut::<ControlsOpen>().0 = false;
        }
    } else if toggle || open_clicked {
        let layout = *world.resource::<ControlsLayout>();
        spawn_screen(&mut world.commands(), layout);
        world.flush();
        world.resource_mut::<ControlsOpen>().0 = true;
        let mut policy = world.resource_mut::<ClosePolicy>();
        if policy.back_closes_primary {
            policy.back_closes_primary = false;
            state.held_back = true;
        }
    }
}

/// A touch-sized button with `caption`, named `name`.
fn touch_button(commands: &mut Commands, name: &str, caption: &str, marker: impl Bundle) -> Entity {
    let min = UiDensity::Comfortable.touch_min();
    let button = commands
        .spawn((
            UiNode,
            NodeName(name.to_owned()),
            NodeStyle::new(Style {
                padding: Edges::px(8.0),
                min_size: [Dimension::Points(min), Dimension::Points(min)],
                justify_content: JustifyContent::Center,
                align_items: Align::Center,
                ..Default::default()
            }),
            Interactable::default(),
            Focusable::default(),
            Button,
            Classes(vec!["button".into()]),
            Themed::default(),
            TouchTarget { min_px: min },
            marker,
        ))
        .id();
    let text = commands
        .spawn((
            UiNode,
            Text {
                spans: vec![TextSpan::lit(caption.to_owned())],
                ..Default::default()
            },
        ))
        .id();
    commands.entity(button).add_child(text);
    button
}

/// The touch layout's open button, top right inside the safe area.
fn spawn_open_button(commands: &mut Commands) {
    let root = commands
        .spawn((
            UiRoot {
                scale_mode: ScaleMode::ConstantPhysicalSize,
                ..UiRoot::default()
            },
            SafeAreaPadding,
            NodeName("controls.open_root".to_owned()),
            NodeStyle::new(Style {
                position: PositionType::Absolute,
                size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                justify_content: JustifyContent::End,
                align_items: Align::Start,
                padding: Edges::px(8.0),
                ..Default::default()
            }),
        ))
        .id();
    let button = touch_button(commands, "controls.open", "Controls", OpenButton);
    commands.entity(root).add_child(button);
}

fn spawn_screen(commands: &mut Commands, layout: ControlsLayout) {
    let touch = layout == ControlsLayout::Touch;
    let mut root = commands.spawn((
        ControlsRoot,
        UiRoot {
            // A phone's reference-resolution scale is far below its density: lay out in dp.
            scale_mode: if touch {
                ScaleMode::ConstantPhysicalSize
            } else {
                UiRoot::default().scale_mode
            },
            // Over the HUD: its corner hint drew across the panel's title on a phone.
            sort_order: if touch { 10 } else { 0 },
            ..UiRoot::default()
        },
        NodeName("controls.root".to_owned()),
        NodeStyle::new(Style {
            position: PositionType::Absolute,
            size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
            justify_content: JustifyContent::Center,
            align_items: Align::Center,
            ..Default::default()
        }),
    ));
    if touch {
        root.insert(SafeAreaPadding);
    }
    let root = root.id();
    // The frosted panel: the game shows through, blurred, under a translucent tint.
    let glass = commands
        .spawn((
            UiNode,
            NodeName("controls.panel".to_owned()),
            BackdropBlur { radius: 12.0 },
            Background {
                kind: BackgroundKind::Color(UiColor([0.05, 0.07, 0.1, 0.55])),
            },
            Border {
                radius: [12.0; 4],
                ..Default::default()
            },
            NodeStyle::new(Style {
                direction: FlexDirection::Column,
                max_size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                ..Default::default()
            }),
        ))
        .id();
    let title = commands
        .spawn((
            UiNode,
            Text {
                spans: vec![TextSpan::lit(
                    if touch {
                        "Controls"
                    } else {
                        "Controls (F10 closes)"
                    }
                    .to_owned(),
                )],
                ..Default::default()
            },
        ))
        .id();
    let panel = commands
        .spawn((
            UiNode,
            NodeName("controls.bindings".to_owned()),
            RebindPanel {
                context: regia_input::GAMEPLAY_CONTEXT.to_owned(),
                actions: Vec::new(),
            },
            RebindPanelOptions {
                per_device: true,
                axis_tuning: true,
                profiles: vec!["player1".to_owned()],
            },
        ))
        .id();
    if touch {
        let header = commands
            .spawn((
                UiNode,
                NodeStyle::new(Style {
                    justify_content: JustifyContent::SpaceBetween,
                    align_items: Align::Center,
                    padding: Edges::px(8.0),
                    ..Default::default()
                }),
            ))
            .id();
        let close = touch_button(commands, "controls.close", "Close", CloseButton);
        commands.entity(header).add_children(&[title, close]);
        // The bindings scroll under a fixed header when the screen is shorter than the list, and
        // sideways when large text makes a row wider than the screen.
        let scroll = commands
            .spawn((
                scroll_container(
                    Style {
                        direction: FlexDirection::Column,
                        flex_grow: 1.0,
                        flex_shrink: 1.0,
                        min_size: [Dimension::Points(0.0), Dimension::Points(0.0)],
                        ..Default::default()
                    },
                    ScrollView::default(),
                ),
                NodeName("controls.scroll".to_owned()),
            ))
            .id();
        commands.entity(scroll).add_child(panel);
        commands.entity(glass).add_children(&[header, scroll]);
    } else {
        commands.entity(glass).add_children(&[title, panel]);
    }
    commands.entity(root).add_child(glass);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> regia_input::InputConfig {
        regia_input::InputConfig::new()
            .action("Look", regia_input::ActionKind::Axis2D)
            .binding(regia_input::Binding::new(
                "Look",
                regia_input::InputSource::MouseDelta,
            ))
            .binding(regia_input::Binding::new(
                "Look",
                regia_input::InputSource::gamepad_stick(regia_input::Stick::Right),
            ))
    }

    fn key(app: &mut App, name: &str, state: ButtonState) {
        app.world_mut().write_message(KeyboardInput {
            window: Entity::PLACEHOLDER,
            physical_key: name.to_owned(),
            logical_key: name.to_owned(),
            state,
            repeat: false,
            timestamp: InputTimestamp::ZERO,
        });
        app.update();
    }

    fn named(app: &mut App, name: &str) -> bool {
        app.world_mut()
            .query::<&NodeName>()
            .iter(app.world())
            .any(|n| n.0 == name)
    }

    fn entity(app: &mut App, name: &str) -> Entity {
        app.world_mut()
            .query::<(Entity, &NodeName)>()
            .iter(app.world())
            .find(|(_, n)| n.0 == name)
            .map(|(e, _)| e)
            .unwrap_or_else(|| panic!("no node named {name}"))
    }

    fn click(app: &mut App, name: &str) {
        let target = entity(app, name);
        app.world_mut()
            .write_message(UiEvent::new(target, UiEventKind::Click));
        app.update();
    }

    fn is_open(app: &App) -> bool {
        app.world().resource::<ControlsOpen>().0
    }

    fn back_quits(app: &App) -> bool {
        app.world().resource::<ClosePolicy>().back_closes_primary
    }

    /// From the TEST Pixel 9 Pro (2026-11-09): a phone has no F10, and the system Back quit the game.
    #[test]
    fn on_a_touch_screen_a_button_opens_the_screen_and_close_or_back_shuts_it_without_quitting() {
        let mut app = App::new();
        app.add_plugins(UiPlugin::code_only())
            .add_message::<WindowResized>()
            .add_message::<WindowScaleFactorChanged>()
            .add_message::<ImeEvent>()
            .add_plugins(regia_input::InputPlugin::new(config()))
            .insert_resource(ControlsLayout::Touch)
            .add_plugins(ControlsPlugin);
        app.update();
        app.update();
        assert!(!is_open(&app) && back_quits(&app));

        click(&mut app, "controls.open");
        assert!(is_open(&app));
        // The rebind panel builds its rows in the frame after the screen spawns.
        app.update();
        for name in [
            "controls.close",
            "controls.scroll",
            "rebind.Gameplay.Look.tuning.gamepad.invert_y",
        ] {
            assert!(named(&mut app, name), "{name}");
        }
        let root = entity(&mut app, "controls.root");
        assert_eq!(
            app.world().get::<UiRoot>(root).map(|r| r.scale_mode),
            Some(ScaleMode::ConstantPhysicalSize)
        );
        assert!(app.world().get::<SafeAreaPadding>(root).is_some());
        app.update();
        assert!(
            !named(&mut app, "controls.open"),
            "the open button would sit on Close"
        );
        assert!(
            !back_quits(&app),
            "Back would quit the game under the open screen"
        );

        // Back closes the screen; the frame that closed it still keeps Back from the window.
        app.world_mut().write_message(BackPressed);
        app.update();
        assert!(!is_open(&app) && !named(&mut app, "controls.panel"));
        assert!(!back_quits(&app));
        app.update();
        assert!(
            back_quits(&app),
            "Back is the game's again once the screen is shut"
        );

        click(&mut app, "controls.open");
        assert!(is_open(&app));
        click(&mut app, "controls.close");
        assert!(!is_open(&app));
        app.update();
        assert!(back_quits(&app));
        // The open button stays for the next time.
        assert!(named(&mut app, "controls.open"));
    }

    #[test]
    fn f10_opens_the_panel_with_its_controls_and_closes_it() {
        let mut app = App::new();
        app.add_plugins(UiPlugin::code_only())
            .add_message::<WindowResized>()
            .add_message::<WindowScaleFactorChanged>()
            .add_message::<ImeEvent>()
            .add_plugins(regia_input::InputPlugin::new(config()))
            .add_plugins(ControlsPlugin);
        app.update();
        assert!(!named(&mut app, "controls.panel"));
        // The desktop layout (the host default) has no on-screen open button.
        assert!(!named(&mut app, "controls.open"));
        key(&mut app, "F10", ButtonState::Pressed);
        key(&mut app, "F10", ButtonState::Released);
        app.update();
        assert!(app.world().resource::<ControlsOpen>().0);
        for name in [
            "controls.panel",
            "rebind.Gameplay.Look.keyboard_mouse",
            "rebind.Gameplay.Look.gamepad",
            "rebind.Gameplay.Look.tuning.gamepad.invert_y",
            "rebind.Gameplay.Look.tuning.gamepad.dead_zone",
            "rebind.Gameplay.profile.player1.save",
        ] {
            assert!(named(&mut app, name), "{name}");
        }
        key(&mut app, "F10", ButtonState::Pressed);
        key(&mut app, "F10", ButtonState::Released);
        app.update();
        assert!(!app.world().resource::<ControlsOpen>().0);
        assert!(!named(&mut app, "controls.panel"));
    }
}
