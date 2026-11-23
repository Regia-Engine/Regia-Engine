// SPDX-License-Identifier: MIT-0
//! HUD (collectible counter, health, damage numbers, status banner, controls hint) and the pause
//! menu. The title and settings panels (`crate::menu`) hang under the same root.

use regia::prelude::*;
use regia_scene::components::Name;
use regia_ui::node::{Background, BackgroundKind, Classes, NodeStyle, UiNode, UiRoot};
use regia_ui::style::{Align, Dimension, FlexDirection, JustifyContent};
use regia_ui::text::{Text, TextAlign, TextOverflow, TextSpan, TextStyle, WrapMode};
use regia_ui::widgets::Button;
use regia_ui::{Edges, SafeAreaPadding, Themed, UiColor};

/// The HUD counter text entity (updated per frame).
#[derive(Resource, Debug, Clone, Copy, Default)]
pub struct HudText(pub Option<Entity>);

/// The counter string the HUD draws (`Coins: 1 / 8`).
///
/// [`Text`] is not a reflected type, so agent `play.expect` checks read this. [`refresh_counter`]
/// writes it from the same `format!` as the text span.
#[derive(Component, Reflect, Debug, Clone, Default, PartialEq, Eq)]
#[reflect(Component, Default)]
pub struct HudLabel {
    /// Literal counter, including the `Coins:` prefix and the total.
    pub text: String,
}

/// The pause-menu root entity (hidden until paused).
#[derive(Resource, Debug, Clone, Copy, Default)]
pub struct PauseMenu(pub Option<Entity>);

/// Spawns the HUD and the (initially hidden) pause menu.
pub fn setup(world: &mut World) {
    let root = world
        .spawn((
            UiRoot::default(),
            SafeAreaPadding,
            NodeStyle::new(regia_ui::style::Style {
                direction: FlexDirection::Column,
                size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                padding: Edges::px(14.0),
                ..Default::default()
            }),
        ))
        .id();

    // Counter, top-left.
    let counter = world
        .spawn((
            UiNode,
            NodeStyle::new(regia_ui::style::Style {
                size: [Dimension::Auto, Dimension::Auto],
                ..Default::default()
            }),
            Background {
                kind: BackgroundKind::Color(UiColor([0.0, 0.0, 0.0, 0.55])),
            },
            Classes(vec!["panel".into()]),
            Text {
                spans: vec![TextSpan::lit("Coins: 0 / 0").with_style(TextStyle {
                    font_px: 26.0,
                    color: UiColor([1.0, 0.95, 0.7, 1.0]),
                    ..Default::default()
                })],
                align: TextAlign::Start,
                overflow: TextOverflow::Ellipsis,
                wrap: WrapMode::Word,
                ..Default::default()
            },
            HudLabel {
                text: "Coins: 0 / 0".into(),
            },
            // The agent resolver reads the ECS `Name`, not the scene-name component.
            regia::prelude::Name::new("hud-counter"),
        ))
        .id();
    world.entity_mut(root).add_child(counter);
    world.insert_resource(HudText(Some(counter)));

    // Health, damage numbers and the status banner under the counter (ADR-2596).
    let health = hud_text(
        world,
        root,
        "HP 100 / 100",
        22.0,
        [0.75, 1.0, 0.75, 1.0],
        "hud-health",
    );
    world.entity_mut(health).insert(HudLabel {
        text: "HP 100 / 100".into(),
    });
    let numbers = hud_text(world, root, "", 20.0, [1.0, 0.85, 0.4, 1.0], "hud-damage-numbers");
    world.entity_mut(numbers).insert(DamageNumbers::default());
    let banner = hud_text(world, root, "", 28.0, [1.0, 1.0, 1.0, 1.0], "hud-banner");
    world.entity_mut(banner).insert(HudLabel::default());
    world.insert_resource(CombatHud {
        health: Some(health),
        numbers: Some(numbers),
        banner: Some(banner),
    });

    // Controls hint, bottom-left.
    let hint = world
        .spawn((
            UiNode,
            Text {
                spans: vec![TextSpan::lit(
                    "WASD move · Space jump (double) · Shift sprint · mouse look · F or left click attack (press again for the combo)\nE car · F2 join player 2 · F5 save · F9 load · Esc pause",
                )
                .with_style(TextStyle {
                    font_px: 16.0,
                    color: UiColor([0.9, 0.92, 0.9, 0.9]),
                    ..Default::default()
                })],
                wrap: WrapMode::Word,
                ..Default::default()
            },
        ))
        .id();
    world.entity_mut(root).add_child(hint);

    spawn_pause_menu(world, root);
    crate::menu::setup(world, root);
}

/// A HUD text line under `parent`.
fn hud_text(world: &mut World, parent: Entity, text: &str, px: f32, color: [f32; 4], name: &str) -> Entity {
    let entity = world
        .spawn((
            UiNode,
            Text {
                spans: vec![TextSpan::lit(text.to_owned()).with_style(TextStyle {
                    font_px: px,
                    color: UiColor(color),
                    ..Default::default()
                })],
                wrap: WrapMode::Word,
                ..Default::default()
            },
            regia::prelude::Name::new(name.to_owned()),
        ))
        .id();
    world.entity_mut(parent).add_child(entity);
    entity
}

/// The combat HUD lines.
#[derive(Resource, Debug, Clone, Copy, Default)]
pub struct CombatHud {
    /// `HP current / max` of player 0.
    pub health: Option<Entity>,
    /// Recent damage numbers.
    pub numbers: Option<Entity>,
    /// Status banner (fallen, checkpoint).
    pub banner: Option<Entity>,
}

/// Recent hits as HUD numbers (reflected: agents read what the HUD shows).
#[derive(Component, Reflect, Debug, Clone, Default, PartialEq)]
#[reflect(Component, Default)]
pub struct DamageNumbers {
    /// Shown entries, newest last: `-12 grunt`, `-14 you`.
    pub entries: Vec<String>,
    /// Seconds each entry has left, parallel to `entries`.
    pub ages: Vec<f32>,
    /// Every number shown so far.
    pub total: u32,
}

/// Seconds a damage number stays on the HUD.
const NUMBER_SECONDS: f32 = 1.2;
/// Most damage numbers shown at once.
const MAX_NUMBERS: usize = 5;

fn set_line(world: &mut World, entity: Option<Entity>, line: &str, px: f32, color: [f32; 4]) {
    let Some(entity) = entity else {
        return;
    };
    if let Some(mut label) = world.get_mut::<HudLabel>(entity) {
        if label.text == line {
            return;
        }
        line.clone_into(&mut label.text);
    }
    if let Some(mut text) = world.get_mut::<Text>(entity) {
        text.spans = vec![TextSpan::lit(line.to_owned()).with_style(TextStyle {
            font_px: px,
            color: UiColor(color),
            ..Default::default()
        })];
    }
}

/// Per-frame: health, damage numbers (from `HitLanded`) and the banner.
#[tracing::instrument(skip_all, target = "third_person", name = "hud_combat")]
pub fn refresh_combat(
    world: &mut World,
    mut cursor: Local<regia_ecs::ecs::message::MessageCursor<crate::combat::HitLanded>>,
) {
    let Some(hud) = world.get_resource::<CombatHud>().copied() else {
        return;
    };
    let dt = world.resource::<Time>().delta_secs();
    let character = crate::input::possessed(world, 0);
    let (current, max) = character
        .map(|c| {
            (
                crate::combat::health(world, c).unwrap_or(0.0),
                world
                    .get::<regia_framework::abilities::Attributes>(c)
                    .and_then(|a| a.get(crate::combat::HEALTH_MAX))
                    .unwrap_or(0.0),
            )
        })
        .unwrap_or((0.0, 0.0));
    let line = format!("HP {} / {}", current.max(0.0).ceil() as i32, max.ceil() as i32);
    let color = if max > 0.0 && current / max < 0.3 {
        [1.0, 0.45, 0.4, 1.0]
    } else {
        [0.75, 1.0, 0.75, 1.0]
    };
    set_line(world, hud.health, &line, 22.0, color);

    // Damage numbers.
    let hits: Vec<crate::combat::HitLanded> = world
        .get_resource::<Messages<crate::combat::HitLanded>>()
        .map(|m| cursor.read(m).copied().collect())
        .unwrap_or_default();
    if let Some(numbers) = hud.numbers {
        let names: Vec<String> = hits
            .iter()
            .map(|h| {
                let who = if h.target_team == crate::combat::TEAM_PLAYER {
                    "you".to_owned()
                } else {
                    world
                        .get::<crate::enemy::Enemy>(h.target)
                        .map_or_else(|| "enemy".to_owned(), |e| format!("{:?}", e.kind).to_lowercase())
                };
                format!("-{} {who}", h.amount.round() as i32)
            })
            .collect();
        let mut changed = !names.is_empty();
        let line = if let Some(mut dn) = world.get_mut::<DamageNumbers>(numbers) {
            for age in &mut dn.ages {
                *age -= dt;
            }
            while dn.ages.first().is_some_and(|a| *a <= 0.0) {
                dn.ages.remove(0);
                dn.entries.remove(0);
                changed = true;
            }
            for name in names {
                dn.entries.push(name);
                dn.ages.push(NUMBER_SECONDS);
                dn.total += 1;
            }
            while dn.entries.len() > MAX_NUMBERS {
                dn.entries.remove(0);
                dn.ages.remove(0);
            }
            changed.then(|| dn.entries.join("   "))
        } else {
            None
        };
        if let Some(line) = line
            && let Some(mut text) = world.get_mut::<Text>(numbers)
        {
            text.spans = vec![TextSpan::lit(line).with_style(TextStyle {
                font_px: 20.0,
                color: UiColor([1.0, 0.85, 0.4, 1.0]),
                ..Default::default()
            })];
        }
    }

    // Banner.
    let downed = character.and_then(|c| world.get::<crate::combat::Downed>(c).copied());
    let toast = world
        .get_resource::<crate::checkpoint::CheckpointToast>()
        .is_some_and(|t| t.0 > 0.0);
    let banner = if let Some(d) = downed {
        format!(
            "You fell. Back to the checkpoint in {:.0}…",
            d.respawn_in.max(0.0).ceil()
        )
    } else if toast {
        "Checkpoint reached".to_owned()
    } else {
        String::new()
    };
    set_line(world, hud.banner, &banner, 28.0, [1.0, 1.0, 1.0, 1.0]);
}

/// The pause menu: a centred panel with Resume / Save / Load / Quit.
fn spawn_pause_menu(world: &mut World, parent: Entity) {
    let menu = world
        .spawn((
            UiNode,
            NodeStyle::new(regia_ui::style::Style {
                direction: FlexDirection::Column,
                size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                justify_content: JustifyContent::Center,
                align_items: Align::Center,
                gap: [Dimension::Points(6.0), Dimension::Points(10.0)],
                // Hidden until Esc / apply_pause. A visible menu would cover the HUD.
                display: regia_ui::style::Display::None,
                ..Default::default()
            }),
            Background {
                kind: BackgroundKind::Color(UiColor([0.0, 0.0, 0.0, 0.6])),
            },
            Themed::default(),
            Name::new("pause-menu"),
            regia::prelude::Name::new("pause-menu"),
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
        ("Settings", PauseAction::Settings),
        ("Quit", PauseAction::Quit),
    ] {
        let button = world
            .spawn((
                UiNode,
                NodeStyle::new(regia_ui::style::Style {
                    padding: Edges::px(10.0),
                    justify_content: JustifyContent::Center,
                    align_items: Align::Center,
                    ..Default::default()
                }),
                regia_ui::node::Interactable::default(),
                regia_ui::focus::Focusable::default(),
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

    world.insert_resource(PauseMenu(Some(menu)));
}

/// Pause-menu button actions.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseAction {
    /// Unpause and hide the menu.
    Resume,
    /// Save the game.
    Save,
    /// Load the game.
    Load,
    /// Open the settings panel.
    Settings,
    /// Exit the app.
    Quit,
}

/// Toggles the pause state: `Time::paused`, the sample state flag, and the menu's
/// visibility (its `hidden` class).
pub fn toggle_pause(world: &mut World) {
    let paused = !world.resource::<Time>().paused;
    apply_pause(world, paused);
}

/// Forces the pause state to `paused` (used by Esc, the menu buttons and the
/// `GameFlow::Paused` enter/exit hooks): freezes gameplay time, mirrors the flag into the sample
/// state, shows/hides the menu, and requests the matching `GameFlow` (a no-op when the flow is
/// already there or not in play yet).
pub fn apply_pause(world: &mut World, paused: bool) {
    world.resource_mut::<Time>().paused = paused;
    regia_framework::flow::request_pause(world, paused);
    if let Some(handle) = world
        .get_resource::<crate::collectible::StateHandle>()
        .and_then(|h| h.0)
        && let Some(mut state) = world.get_mut::<crate::SampleState>(handle)
    {
        state.paused = paused;
    }
    // The menu is visible exactly while paused.
    if let Some(menu) = world.get_resource::<PauseMenu>().and_then(|p| p.0)
        && let Some(mut node) = world.get_mut::<NodeStyle>(menu)
    {
        node.0.display = if paused {
            regia_ui::style::Display::Flex
        } else {
            regia_ui::style::Display::None
        };
    }
}

/// Per-frame: refresh the counter text from the sample state.
#[tracing::instrument(skip_all, target = "third_person", name = "hud_refresh")]
pub fn refresh_counter(world: &mut World) {
    let Some(text_entity) = world.get_resource::<HudText>().and_then(|h| h.0) else {
        return;
    };
    let Some(handle) = world
        .get_resource::<crate::collectible::StateHandle>()
        .and_then(|h| h.0)
    else {
        return;
    };
    let Some(state) = world.get::<crate::SampleState>(handle).copied() else {
        return;
    };
    let label = format!("Coins: {} / {}", state.collected, state.total);
    if let Some(mut text) = world.get_mut::<Text>(text_entity) {
        text.spans[0] = TextSpan::lit(label.clone()).with_style(TextStyle {
            font_px: 26.0,
            color: UiColor([1.0, 0.95, 0.7, 1.0]),
            ..Default::default()
        });
    }
    if let Some(mut hud) = world.get_mut::<HudLabel>(text_entity) {
        hud.text = label;
    }
}

/// Handles pause-menu button clicks (pointer UI events on the button entities).
#[tracing::instrument(skip_all, target = "third_person", name = "pause_menu_input")]
pub fn menu_clicks(world: &mut World) {
    if !world.contains_resource::<Messages<regia_ui::interaction::UiEvent>>() {
        return; // no UI interaction (headless tests)
    }
    let mut cursors = world
        .remove_resource::<crate::input::InputCursors>()
        .unwrap_or_default();
    let events: Vec<regia_ui::interaction::UiEvent> = world
        .get_resource::<Messages<regia_ui::interaction::UiEvent>>()
        .map(|messages| cursors.ui.read(messages).cloned().collect())
        .unwrap_or_default();
    world.insert_resource(cursors);
    for event in events {
        if !matches!(event.kind, regia_ui::interaction::UiEventKind::Click) {
            continue;
        }
        let Some(action) = world.get::<PauseAction>(event.target).copied() else {
            continue;
        };
        match action {
            PauseAction::Resume => apply_pause(world, false),
            PauseAction::Save => crate::save::save_now(world),
            PauseAction::Load => crate::save::load_now(world),
            PauseAction::Settings => crate::menu::apply_action(world, crate::menu::MenuAction::OpenSettings),
            PauseAction::Quit => {
                world.write_message(AppExit::Success);
            }
        }
    }
    let _ = Button;
    let _ = JustifyContent::Center;
}
