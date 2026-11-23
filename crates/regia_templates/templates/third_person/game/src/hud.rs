// SPDX-License-Identifier: MIT-0
//! The HUD (a collectible counter, health and enemy counts, a controls hint, a "You died" banner),
//! a save/load toast, and the pause menu.
//!
//! Agents read the counter through [`HudLabel`] (`Text` is not a reflected type), the same string
//! the screen shows. Esc pauses: the game flow enters `GameFlow::Paused` (which freezes gameplay
//! time) and the menu appears; its Save / Load buttons send the framework's save requests (spec 91).

use regia::prelude::*;
use regia::ui::interaction::{UiEvent, UiEventKind};
use regia::ui::node::{Background, BackgroundKind, Classes, NodeStyle, UiNode, UiRoot};
use regia::ui::style::{Align, Dimension, Display, FlexDirection, JustifyContent};
use regia::ui::text::{Text, TextAlign, TextOverflow, TextSpan, TextStyle, WrapMode};
use regia::ui::widgets::Button;
use regia::ui::{Edges, SafeAreaPadding, Themed, UiColor};
use regia_framework::flow::GameFlow;
use regia_framework::save_flow::{SaveOp, SaveOutcome, SaveRequest};
use regia_input::prelude::*;
use regia_scene::components::Name as SceneName;

use crate::collectible::{CollectProgress, total};
use crate::combat::Health;
use crate::enemy::CombatProgress;
use crate::menu::MenuAction;
use crate::respawn::Respawn;

/// Seconds a "Saved" / "Loaded" toast stays on screen.
const TOAST_SECS: f32 = 2.0;

/// The save slot the pause menu's buttons use.
pub const MENU_SLOT: &str = "Chapter1";

/// The counter string the HUD draws (`Coins: 1 / 4`), readable by agents.
#[derive(Component, Reflect, Debug, Clone, Default, PartialEq, Eq)]
#[reflect(Component, Default)]
pub struct HudLabel {
    /// Literal counter, including the `Coins:` prefix and the total.
    pub text: String,
}

/// The toast text currently on screen, readable by agents; empty when none.
#[derive(Component, Reflect, Debug, Clone, Default, PartialEq, Eq)]
#[reflect(Component, Default)]
pub struct ToastLabel {
    /// The toast message.
    pub text: String,
}

/// The health line the HUD draws (`Health: 100 / 100`), readable by agents.
#[derive(Component, Reflect, Debug, Clone, Default, PartialEq, Eq)]
#[reflect(Component, Default)]
pub struct CombatLabel {
    /// Literal line.
    pub text: String,
}

/// The enemy count the HUD draws (`Enemies: 0 / 3`), readable by agents.
#[derive(Component, Reflect, Debug, Clone, Default, PartialEq, Eq)]
#[reflect(Component, Default)]
pub struct EnemiesLabel {
    /// Literal line.
    pub text: String,
}

/// The big message in the middle of the screen (`You died`; empty when none), readable by agents.
#[derive(Component, Reflect, Debug, Clone, Default, PartialEq, Eq)]
#[reflect(Component, Default)]
pub struct BannerLabel {
    /// The message.
    pub text: String,
}

/// The entities and timer the HUD systems share.
#[derive(Resource, Debug, Clone, Copy, Default)]
pub struct HudEntities {
    counter: Option<Entity>,
    combat: Option<Entity>,
    enemies: Option<Entity>,
    banner: Option<Entity>,
    toast: Option<Entity>,
    menu: Option<Entity>,
    toast_left: f32,
}

/// Pause-menu button actions.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseAction {
    /// Unpause and hide the menu.
    Resume,
    /// Save to [`MENU_SLOT`].
    Save,
    /// Load [`MENU_SLOT`].
    Load,
    /// Exit the game.
    Quit,
}

fn label_style(font_px: f32, color: [f32; 4]) -> TextStyle {
    TextStyle {
        font_px,
        color: UiColor(color),
        ..Default::default()
    }
}

fn counter_text(collected: usize) -> String {
    format!("Coins: {collected} / {}", total())
}

/// `Startup`: the HUD, the toast line and the (hidden) pause menu.
pub fn setup_hud(world: &mut World) {
    let root = world
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

    let text = counter_text(0);
    let counter = world
        .spawn((
            UiNode,
            NodeStyle::new(regia::ui::style::Style {
                size: [Dimension::Auto, Dimension::Auto],
                ..Default::default()
            }),
            Background {
                kind: BackgroundKind::Color(UiColor([0.0, 0.0, 0.0, 0.55])),
            },
            Classes(vec!["panel".into()]),
            Text {
                spans: vec![TextSpan::lit(text.clone()).with_style(label_style(26.0, [1.0, 0.95, 0.7, 1.0]))],
                align: TextAlign::Start,
                overflow: TextOverflow::Ellipsis,
                wrap: WrapMode::Word,
                ..Default::default()
            },
            HudLabel { text },
            // The agent resolver reads the ECS `Name`, not the scene-name component.
            Name::new("hud-counter"),
        ))
        .id();
    world.entity_mut(root).add_child(counter);

    let initial = health_text(Health::default().current);
    let enemies_initial = enemies_text(0);
    let combat = world
        .spawn((
            UiNode,
            Text {
                spans: vec![
                    TextSpan::lit(initial.clone()).with_style(label_style(22.0, [1.0, 0.6, 0.55, 1.0])),
                ],
                ..Default::default()
            },
            CombatLabel { text: initial },
            Name::new("hud-combat"),
        ))
        .id();
    world.entity_mut(root).add_child(combat);
    let enemies = world
        .spawn((
            UiNode,
            Text {
                spans: vec![
                    TextSpan::lit(enemies_initial.clone())
                        .with_style(label_style(22.0, [1.0, 0.6, 0.55, 1.0])),
                ],
                ..Default::default()
            },
            EnemiesLabel {
                text: enemies_initial,
            },
            Name::new("hud-enemies"),
        ))
        .id();
    world.entity_mut(root).add_child(enemies);

    let banner = world
        .spawn((
            UiNode,
            Text {
                spans: vec![
                    TextSpan::lit(String::new()).with_style(label_style(48.0, [1.0, 0.3, 0.25, 1.0])),
                ],
                ..Default::default()
            },
            BannerLabel::default(),
            Name::new("hud-banner"),
        ))
        .id();
    world.entity_mut(root).add_child(banner);

    let toast = world
        .spawn((
            UiNode,
            Text {
                spans: vec![TextSpan::lit(String::new()).with_style(label_style(22.0, [0.8, 1.0, 0.8, 1.0]))],
                ..Default::default()
            },
            ToastLabel::default(),
            Name::new("hud-toast"),
        ))
        .id();
    world.entity_mut(root).add_child(toast);

    let hint = world
        .spawn((
            UiNode,
            Text {
                spans: vec![
                    TextSpan::lit("WASD move · Space jump · mouse look · J / left click attack\nF5 quicksave · F9 quickload · Esc pause")
                        .with_style(label_style(16.0, [0.9, 0.92, 0.9, 0.9])),
                ],
                wrap: WrapMode::Word,
                ..Default::default()
            },
        ))
        .id();
    world.entity_mut(root).add_child(hint);

    let menu = spawn_pause_menu(world, root);
    // The title and settings panels come last so they draw over the pause menu.
    crate::menu::setup(world, root);
    world.insert_resource(HudEntities {
        counter: Some(counter),
        combat: Some(combat),
        enemies: Some(enemies),
        banner: Some(banner),
        toast: Some(toast),
        menu: Some(menu),
        toast_left: 0.0,
    });
}

/// The pause menu: a centered panel with Resume / Save / Load / Quit, hidden until paused.
fn spawn_pause_menu(world: &mut World, parent: Entity) -> Entity {
    let menu = world
        .spawn((
            UiNode,
            NodeStyle::new(regia::ui::style::Style {
                direction: FlexDirection::Column,
                size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                justify_content: JustifyContent::Center,
                align_items: Align::Center,
                gap: [Dimension::Points(6.0), Dimension::Points(10.0)],
                display: Display::None,
                ..Default::default()
            }),
            Background {
                kind: BackgroundKind::Color(UiColor([0.0, 0.0, 0.0, 0.6])),
            },
            Themed::default(),
            SceneName::new("pause-menu"),
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
                Button,
                Classes(vec!["button".into()]),
                Themed::default(),
                action,
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

/// `Update`: Esc toggles between `Playing` and `Paused`.
pub fn toggle_pause(
    actions: Res<Actions>,
    flow: Res<State<GameFlow>>,
    mut next: ResMut<NextState<GameFlow>>,
) {
    if !actions.just_pressed_this_frame("Pause") {
        return;
    }
    match flow.get() {
        GameFlow::Playing => next.set(GameFlow::Paused),
        GameFlow::Paused => next.set(GameFlow::Playing),
        _ => {}
    }
}

fn health_text(health: f32) -> String {
    format!("Health: {health:.0} / {:.0}", crate::combat::PLAYER_HEALTH)
}

fn enemies_text(defeated: usize) -> String {
    format!("Enemies: {defeated} / {}", crate::enemy::total())
}

/// `Update`: refreshes the health line and the enemy count.
#[tracing::instrument(skip_all, name = "hud_refresh_combat")]
pub fn refresh_combat(
    players: Query<&Health, With<regia_framework::character::Character>>,
    progress: Res<CombatProgress>,
    hud: Res<HudEntities>,
    mut texts: Query<&mut Text>,
    mut combat: Query<&mut CombatLabel>,
    mut enemies: Query<&mut EnemiesLabel>,
) {
    let health = players.iter().next().map_or(0.0, |health| health.current);
    let line = health_text(health);
    if let Some(entity) = hud.combat
        && let Ok(mut label) = combat.get_mut(entity)
        && label.text != line
    {
        label.text.clone_from(&line);
        if let Ok(mut text) = texts.get_mut(entity) {
            text.spans[0] = TextSpan::lit(line).with_style(label_style(22.0, [1.0, 0.6, 0.55, 1.0]));
        }
    }
    let count = enemies_text(progress.defeated.len());
    if let Some(entity) = hud.enemies
        && let Ok(mut label) = enemies.get_mut(entity)
        && label.text != count
    {
        label.text.clone_from(&count);
        if let Ok(mut text) = texts.get_mut(entity) {
            text.spans[0] = TextSpan::lit(count).with_style(label_style(22.0, [1.0, 0.6, 0.55, 1.0]));
        }
    }
}

/// `Update`: shows "You died" while the respawn countdown runs.
#[tracing::instrument(skip_all, name = "hud_refresh_banner")]
pub fn refresh_banner(
    respawn: Res<Respawn>,
    hud: Res<HudEntities>,
    mut texts: Query<&mut Text>,
    mut banner: Query<&mut BannerLabel>,
) {
    let message = if respawn.left.is_some() { "You died" } else { "" };
    if let Some(entity) = hud.banner
        && let Ok(mut label) = banner.get_mut(entity)
        && label.text != message
    {
        message.clone_into(&mut label.text);
        if let Ok(mut text) = texts.get_mut(entity) {
            text.spans[0] =
                TextSpan::lit(message.to_owned()).with_style(label_style(48.0, [1.0, 0.3, 0.25, 1.0]));
        }
    }
}

/// `Update`: refreshes the counter from the collect progress.
#[tracing::instrument(skip_all, name = "hud_refresh")]
pub fn refresh_counter(
    progress: Res<CollectProgress>,
    hud: Res<HudEntities>,
    mut texts: Query<&mut Text>,
    mut labels: Query<&mut HudLabel>,
) {
    let Some(entity) = hud.counter else {
        return;
    };
    let label = counter_text(progress.collected.len());
    if let Ok(mut hud_label) = labels.get_mut(entity) {
        if hud_label.text == label {
            return;
        }
        hud_label.text.clone_from(&label);
    }
    if let Ok(mut text) = texts.get_mut(entity) {
        text.spans[0] = TextSpan::lit(label).with_style(label_style(26.0, [1.0, 0.95, 0.7, 1.0]));
    }
}

/// `Update`: shows a toast for each finished save or load, and hides it after `TOAST_SECS`.
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
        text.spans[0] = TextSpan::lit(shown.to_owned()).with_style(label_style(22.0, [0.8, 1.0, 0.8, 1.0]));
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

/// Where `menu_clicks` has read up to in the UI event stream.
#[derive(Resource, Default)]
pub struct MenuCursor(regia::ecs::ecs::message::MessageCursor<UiEvent>);

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
