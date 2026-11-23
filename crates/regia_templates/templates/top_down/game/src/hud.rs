// SPDX-License-Identifier: MIT-0
//! The HUD: two status lines (player health, and the enemy), a controls hint and a save/load toast.
//!
//! Agents read the strings through [`StatusLabel`], [`EnemyLabel`] and [`ToastLabel`] (`Text` is not a reflected
//! type); they are the same strings the screen shows.

use regia::prelude::*;
use regia::ui::node::{NodeStyle, UiNode, UiRoot};
use regia::ui::style::{Dimension, FlexDirection};
use regia::ui::text::{Text, TextSpan, TextStyle, WrapMode};
use regia::ui::{Edges, SafeAreaPadding, UiColor};
use regia_framework::character::Character;
use regia_framework::save_flow::{SaveOp, SaveOutcome};

use crate::combat::Health;
use crate::enemy::{CombatProgress, Enemy};

/// Seconds a "Saved" / "Loaded" toast stays on screen.
const TOAST_SECS: f32 = 2.0;

/// The player's health line (`Health: 100 / 100`), readable by agents.
#[derive(Component, Reflect, Debug, Clone, Default, PartialEq, Eq)]
#[reflect(Component, Default)]
pub struct StatusLabel {
    /// Literal status line.
    pub text: String,
}

/// The enemy line (`Enemy: 36`, or `Enemy: down`), readable by agents.
#[derive(Component, Reflect, Debug, Clone, Default, PartialEq, Eq)]
#[reflect(Component, Default)]
pub struct EnemyLabel {
    /// Literal enemy line.
    pub text: String,
}

/// The toast text currently on screen; empty when none.
#[derive(Component, Reflect, Debug, Clone, Default, PartialEq, Eq)]
#[reflect(Component, Default)]
pub struct ToastLabel {
    /// The toast message.
    pub text: String,
}

/// The HUD entities and the toast timer.
#[derive(Resource, Debug, Clone, Copy, Default)]
pub struct HudEntities {
    status: Option<Entity>,
    enemy: Option<Entity>,
    toast: Option<Entity>,
    toast_left: f32,
}

fn style(font_px: f32, color: [f32; 4]) -> TextStyle {
    TextStyle {
        font_px,
        color: UiColor(color),
        ..Default::default()
    }
}

fn health_line(health: f32) -> String {
    format!("Health: {health:.0} / {:.0}", crate::combat::PLAYER_HEALTH)
}

fn enemy_line(enemy: Option<f32>) -> String {
    match enemy {
        Some(hp) => format!("Enemy: {hp:.0}"),
        None => "Enemy: down".to_owned(),
    }
}

/// `Startup`: the status line, the toast and the hint.
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
    let text = health_line(crate::combat::PLAYER_HEALTH);
    let enemy_text = enemy_line(Some(crate::enemy::ENEMY_HEALTH));
    let status = world
        .spawn((
            UiNode,
            Text {
                spans: vec![TextSpan::lit(text.clone()).with_style(style(24.0, [1.0, 0.8, 0.7, 1.0]))],
                ..Default::default()
            },
            StatusLabel { text },
            Name::new("hud-status"),
        ))
        .id();
    let enemy = world
        .spawn((
            UiNode,
            Text {
                spans: vec![TextSpan::lit(enemy_text.clone()).with_style(style(24.0, [1.0, 0.8, 0.7, 1.0]))],
                ..Default::default()
            },
            EnemyLabel { text: enemy_text },
            Name::new("hud-enemy"),
        ))
        .id();
    let toast = world
        .spawn((
            UiNode,
            Text {
                spans: vec![TextSpan::lit(String::new()).with_style(style(22.0, [0.8, 1.0, 0.8, 1.0]))],
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
                    TextSpan::lit("Click or WASD move · Q / E turn view · F / right click attack\nF5 quicksave · F9 quickload")
                        .with_style(style(16.0, [0.9, 0.92, 0.9, 0.9])),
                ],
                wrap: WrapMode::Word,
                ..Default::default()
            },
        ))
        .id();
    for child in [status, enemy, toast, hint] {
        world.entity_mut(root).add_child(child);
    }
    world.insert_resource(HudEntities {
        status: Some(status),
        enemy: Some(enemy),
        toast: Some(toast),
        toast_left: 0.0,
    });
}

/// `Update`: refreshes the health and enemy lines.
#[tracing::instrument(skip_all, name = "hud_refresh")]
pub fn refresh_status(
    players: Query<&Health, With<Character>>,
    enemies: Query<&Health, With<Enemy>>,
    progress: Res<CombatProgress>,
    hud: Res<HudEntities>,
    mut texts: Query<&mut Text>,
    mut labels: Query<&mut StatusLabel>,
    mut enemy_labels: Query<&mut EnemyLabel>,
) {
    let player = players.iter().next().map_or(0.0, |health| health.current);
    let status = health_line(player);
    if let Some(entity) = hud.status
        && let Ok(mut label) = labels.get_mut(entity)
        && label.text != status
    {
        label.text.clone_from(&status);
        if let Ok(mut shown) = texts.get_mut(entity) {
            shown.spans[0] = TextSpan::lit(status).with_style(style(24.0, [1.0, 0.8, 0.7, 1.0]));
        }
    }
    let enemy = if progress.enemy_defeated {
        None
    } else {
        enemies.iter().next().map(|health| health.current)
    };
    let text = enemy_line(enemy);
    if let Some(entity) = hud.enemy
        && let Ok(mut label) = enemy_labels.get_mut(entity)
        && label.text != text
    {
        label.text.clone_from(&text);
        if let Ok(mut shown) = texts.get_mut(entity) {
            shown.spans[0] = TextSpan::lit(text).with_style(style(24.0, [1.0, 0.8, 0.7, 1.0]));
        }
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
        text.spans[0] = TextSpan::lit(shown.to_owned()).with_style(style(22.0, [0.8, 1.0, 0.8, 1.0]));
    }
    if let Ok(mut label) = labels.get_mut(entity) {
        label.text = shown.to_owned();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_status_lines_name_both_sides() {
        assert_eq!(health_line(80.0), "Health: 80 / 100");
        assert_eq!(enemy_line(Some(12.0)), "Enemy: 12");
        assert_eq!(enemy_line(None), "Enemy: down");
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
