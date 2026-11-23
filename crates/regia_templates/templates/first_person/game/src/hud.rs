// SPDX-License-Identifier: MIT-0
//! The HUD: a crosshair, the interaction prompt, a save/load toast and the pause menu.
//!
//! Agents read the prompt and the toast through [`PromptLabel`] and [`ToastLabel`] (`Text` is not
//! a reflected type); the strings are the ones on screen. Esc pauses: the game flow enters
//! `GameFlow::Paused`, which freezes gameplay time, and the menu appears; Save and Load send the
//! framework's save requests (spec 91) and Settings opens the panel from `menu.rs`.

use regia::prelude::*;
use regia::ui::interaction::{UiEvent, UiEventKind};
use regia::ui::node::{Background, BackgroundKind, Classes, NodeStyle, UiNode, UiRoot};
use regia::ui::style::{Align, Dimension, Display, FlexDirection, JustifyContent};
use regia::ui::text::{Text, TextAlign, TextSpan, TextStyle, WrapMode};
use regia::ui::widgets::Button as UiButton;
use regia::ui::{Edges, SafeAreaPadding, Themed, UiColor};
use regia_framework::flow::GameFlow;
use regia_framework::save_flow::{SaveOp, SaveOutcome, SaveRequest};
use regia_input::prelude::*;

use crate::interact::{Button, Door, Focus, Interactable};
use crate::menu::MenuAction;

/// Seconds a "Saved" / "Loaded" toast stays on screen.
const TOAST_SECS: f32 = 2.0;
/// The save slot the pause menu's buttons use.
pub const MENU_SLOT: &str = "Chapter1";

/// The prompt string (`Press E to open the gate`; empty when nothing is in reach), readable by agents.
#[derive(Component, Reflect, Debug, Clone, Default, PartialEq, Eq)]
#[reflect(Component, Default)]
pub struct PromptLabel {
    /// The prompt.
    pub text: String,
}

/// The toast text currently on screen; empty when none.
#[derive(Component, Reflect, Debug, Clone, Default, PartialEq, Eq)]
#[reflect(Component, Default)]
pub struct ToastLabel {
    /// The toast message.
    pub text: String,
}

/// The crosshair node (named `crosshair` for agents).
#[derive(Component, Debug, Clone, Copy)]
pub struct Crosshair;

/// Pause-menu button actions.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseAction {
    /// Unpause.
    Resume,
    /// Save to [`MENU_SLOT`].
    Save,
    /// Load [`MENU_SLOT`].
    Load,
    /// Exit the game.
    Quit,
}

/// The HUD entities and the toast timer.
#[derive(Resource, Debug, Clone, Copy, Default)]
pub struct HudEntities {
    prompt: Option<Entity>,
    toast: Option<Entity>,
    menu: Option<Entity>,
    toast_left: f32,
}

/// Where `menu_clicks` has read up to in the UI event stream.
#[derive(Resource, Default)]
pub struct MenuCursor(regia::ecs::ecs::message::MessageCursor<UiEvent>);

fn text_style(font_px: f32, color: [f32; 4]) -> TextStyle {
    TextStyle {
        font_px,
        color: UiColor(color),
        ..Default::default()
    }
}

fn centered_column(gap: f32) -> NodeStyle {
    NodeStyle::new(regia::ui::style::Style {
        direction: FlexDirection::Column,
        size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
        justify_content: JustifyContent::Center,
        align_items: Align::Center,
        gap: [Dimension::Points(6.0), Dimension::Points(gap)],
        ..Default::default()
    })
}

/// `Startup`: the crosshair and prompt in the middle, the toast and hint in a corner, the
/// (hidden) pause menu and settings panel on top.
pub fn setup_hud(world: &mut World) {
    // Middle of the screen: the crosshair, then the prompt under it.
    let middle = world
        .spawn((
            UiRoot::default(),
            SafeAreaPadding,
            centered_column(14.0),
            Name::new("hud-middle"),
        ))
        .id();
    let crosshair = world
        .spawn((
            UiNode,
            NodeStyle::new(regia::ui::style::Style {
                size: [Dimension::Points(8.0), Dimension::Points(8.0)],
                ..Default::default()
            }),
            Background {
                kind: BackgroundKind::Color(UiColor([1.0, 1.0, 1.0, 0.85])),
            },
            Crosshair,
            Name::new("crosshair"),
        ))
        .id();
    let prompt = world
        .spawn((
            UiNode,
            Text {
                spans: vec![TextSpan::lit(String::new()).with_style(text_style(24.0, [1.0, 0.95, 0.7, 1.0]))],
                align: TextAlign::Center,
                ..Default::default()
            },
            PromptLabel::default(),
            Name::new("hud-prompt"),
        ))
        .id();
    world.entity_mut(middle).add_child(crosshair);
    world.entity_mut(middle).add_child(prompt);

    // Corner: the toast and the controls hint.
    let corner = world
        .spawn((
            UiRoot::default(),
            SafeAreaPadding,
            NodeStyle::new(regia::ui::style::Style {
                direction: FlexDirection::Column,
                size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                padding: Edges::px(14.0),
                ..Default::default()
            }),
        ))
        .id();
    let toast = world
        .spawn((
            UiNode,
            Text {
                spans: vec![TextSpan::lit(String::new()).with_style(text_style(22.0, [0.8, 1.0, 0.8, 1.0]))],
                ..Default::default()
            },
            ToastLabel::default(),
            Name::new("hud-toast"),
        ))
        .id();
    let hint = world
        .spawn((
            UiNode,
            Text {
                spans: vec![
                    TextSpan::lit("WASD move · mouse look · E use\nF5 quicksave · F9 quickload · Esc pause · F10 controls")
                        .with_style(text_style(16.0, [0.9, 0.92, 0.9, 0.9])),
                ],
                wrap: WrapMode::Word,
                ..Default::default()
            },
        ))
        .id();
    world.entity_mut(corner).add_child(toast);
    world.entity_mut(corner).add_child(hint);

    let menu = spawn_pause_menu(world, corner);
    // The settings panel comes last so it draws over the pause menu.
    crate::menu::setup(world, corner);
    world.insert_resource(HudEntities {
        prompt: Some(prompt),
        toast: Some(toast),
        menu: Some(menu),
        toast_left: 0.0,
    });
}

/// The pause menu: a centered panel with Resume / Save / Load / Settings / Quit, hidden until paused.
fn spawn_pause_menu(world: &mut World, parent: Entity) -> Entity {
    let menu = world
        .spawn((
            UiNode,
            {
                let mut style = centered_column(10.0);
                style.0.display = Display::None;
                style
            },
            Background {
                kind: BackgroundKind::Color(UiColor([0.0, 0.0, 0.0, 0.6])),
            },
            Themed::default(),
            Name::new("pause-menu"),
        ))
        .id();
    world.entity_mut(parent).add_child(menu);
    let title = world
        .spawn((
            UiNode,
            Text {
                spans: vec![TextSpan::lit("Paused").with_style(TextStyle {
                    font_px: 34.0,
                    ..Default::default()
                })],
                ..Default::default()
            },
        ))
        .id();
    world.entity_mut(menu).add_child(title);
    for (label, action) in [
        ("Resume", PauseAction::Resume),
        ("Save", PauseAction::Save),
        ("Load", PauseAction::Load),
        ("Quit", PauseAction::Quit),
    ] {
        let button = world
            .spawn((
                UiNode,
                NodeStyle::new(regia::ui::style::Style {
                    padding: Edges::px(10.0),
                    justify_content: JustifyContent::Center,
                    align_items: Align::Center,
                    ..Default::default()
                }),
                regia::ui::node::Interactable::default(),
                regia::ui::focus::Focusable::default(),
                UiButton,
                Classes(vec!["button".into()]),
                Themed::default(),
                action,
                Name::new(format!("pause-{}", label.to_lowercase())),
            ))
            .id();
        let caption = world
            .spawn((
                UiNode,
                Text {
                    spans: vec![TextSpan::lit(label.to_owned())],
                    wrap: WrapMode::None,
                    ..Default::default()
                },
            ))
            .id();
        world.entity_mut(button).add_child(caption);
        world.entity_mut(menu).add_child(button);
    }
    crate::menu::button(
        world,
        menu,
        "Settings",
        "pause-settings",
        MenuAction::OpenSettings,
    );
    menu
}

/// Shows or hides the pause menu. (The game flow owns the pause itself: entering
/// `GameFlow::Paused` stops `Time`; see `lib.rs`.)
pub fn show_pause(world: &mut World, paused: bool) {
    let menu = world.get_resource::<HudEntities>().and_then(|h| h.menu);
    if let Some(menu) = menu
        && let Some(mut node) = world.get_mut::<NodeStyle>(menu)
    {
        node.0.display = if paused { Display::Flex } else { Display::None };
    }
}

/// `Update`: Esc toggles between `Playing` and `Paused` (unless the controls screen is open).
pub fn toggle_pause(
    actions: Res<Actions>,
    controls: Res<crate::controls::ControlsOpen>,
    flow: Res<State<GameFlow>>,
    mut next: ResMut<NextState<GameFlow>>,
) {
    if !actions.just_pressed_this_frame("Pause") || controls.0 {
        return;
    }
    match flow.get() {
        GameFlow::Playing => next.set(GameFlow::Paused),
        GameFlow::Paused => next.set(GameFlow::Playing),
        _ => {}
    }
}

/// `Update`: writes the prompt for the focused interactable.
#[tracing::instrument(skip_all, name = "hud_prompt")]
pub fn refresh_prompt(
    focus: Res<Focus>,
    things: Query<(&Interactable, Option<&Door>, Option<&Button>)>,
    hud: Res<HudEntities>,
    mut texts: Query<&mut Text>,
    mut labels: Query<&mut PromptLabel>,
) {
    let Some(entity) = hud.prompt else {
        return;
    };
    let text = focus
        .0
        .and_then(|target| things.get(target).ok())
        .map(|(interactable, door, button)| {
            let on = door.is_some_and(|d| d.open) || button.is_some_and(|b| b.on);
            crate::interact::prompt_for(interactable, on)
        })
        .unwrap_or_default();
    let Ok(mut label) = labels.get_mut(entity) else {
        return;
    };
    if label.text == text {
        return;
    }
    label.text.clone_from(&text);
    if let Ok(mut shown) = texts.get_mut(entity) {
        shown.spans[0] = TextSpan::lit(text).with_style(text_style(24.0, [1.0, 0.95, 0.7, 1.0]));
    }
}

/// `Update`: a toast for each finished save or load, hidden after `TOAST_SECS`.
pub fn toast(
    mut outcomes: MessageReader<SaveOutcome>,
    time: Res<Time>,
    mut hud: ResMut<HudEntities>,
    mut texts: Query<&mut Text>,
    mut labels: Query<&mut ToastLabel>,
) {
    let mut message = None;
    for outcome in outcomes.read() {
        message = save_message(outcome.op, outcome.result.is_ok()).or(message);
    }
    if message.is_none() {
        if hud.toast_left <= 0.0 {
            return;
        }
        hud.toast_left -= time.delta_secs();
        if hud.toast_left > 0.0 {
            return;
        }
    } else {
        hud.toast_left = TOAST_SECS;
    }
    let shown = message.unwrap_or_default();
    let Some(entity) = hud.toast else {
        return;
    };
    if let Ok(mut text) = texts.get_mut(entity) {
        text.spans[0] = TextSpan::lit(shown.to_owned()).with_style(text_style(22.0, [0.8, 1.0, 0.8, 1.0]));
    }
    if let Ok(mut label) = labels.get_mut(entity) {
        label.text = shown.to_owned();
    }
}

/// `Update`: pause-menu button clicks.
pub fn menu_clicks(world: &mut World) {
    if !world.contains_resource::<Messages<UiEvent>>() {
        return; // no UI plugin (headless tests)
    }
    let mut cursor = world.remove_resource::<MenuCursor>().unwrap_or_default();
    let clicks: Vec<Entity> = world
        .get_resource::<Messages<UiEvent>>()
        .map(|messages| {
            cursor
                .0
                .read(messages)
                .filter(|event| matches!(event.kind, UiEventKind::Click))
                .map(|event| event.target)
                .collect()
        })
        .unwrap_or_default();
    world.insert_resource(cursor);
    for target in clicks {
        let Some(action) = world.get::<PauseAction>(target).copied() else {
            continue;
        };
        match action {
            PauseAction::Resume => {
                world.resource_mut::<NextState<GameFlow>>().set(GameFlow::Playing);
            }
            PauseAction::Save => {
                world.write_message(SaveRequest::Save {
                    slot: MENU_SLOT.into(),
                });
            }
            PauseAction::Load => {
                world.write_message(SaveRequest::Load {
                    slot: MENU_SLOT.into(),
                });
            }
            PauseAction::Quit => {
                world.write_message(AppExit::Success);
            }
        }
    }
}

/// The operation in a failure toast must match the operation the player requested.
fn save_message(op: SaveOp, ok: bool) -> Option<&'static str> {
    match (op, ok) {
        (SaveOp::Save, true) => Some("Saved"),
        (SaveOp::Load, true) => Some("Loaded"),
        (SaveOp::Load, false) => Some("Load failed"),
        (_, false) => Some("Save failed"),
        _ => None,
    }
}

#[cfg(test)]
mod save_toast_tests {
    use super::*;

    #[test]
    fn failed_load_says_load_failed() {
        assert_eq!(save_message(SaveOp::Load, false), Some("Load failed"));
        assert_eq!(save_message(SaveOp::Save, false), Some("Save failed"));
    }
}
