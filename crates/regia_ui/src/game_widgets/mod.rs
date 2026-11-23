// SPDX-License-Identifier: MPL-2.0
//! Reusable game UI widgets (ADR-2958, `book/user-guide/game-widgets.md`,
//! `book/agents/ui-game-widgets.md`): a lobby list, a scoreboard, a radar and minimap, and world
//! markers and target brackets with lead.
//!
//! Every widget is a component on a node, drawn with the existing quad kinds, and is fed plain data:
//!
//! - [`LobbyList`] shows rows the game (or `regia_framework::game_ui`, from `regia_net::Lobbies`)
//!   writes; its buttons send [`LobbyListRequest`] messages.
//! - [`Scoreboard`] shows a [`ScoreTable`] (`regia_framework::game_ui` fills it from
//!   `GameRules::scoreboard`).
//! - [`Radar`] shows every [`RadarBlip`] entity around a center, or over a whole map.
//! - [`MarkerLayer`] shows every [`WorldMarker`] and [`TargetLock`] entity through a camera. A marker
//!   with a [`WorldPlate`] instead gets its own world-space root ([`crate::world_target`]).
//!
//! This crate knows neither `regia_net` nor `regia_framework`, so a game without networking or the
//! framework can still use all of them.
//!
//! Everything that moves every frame is placed by [`crate::UiTransform`], which layout never sees,
//! so 500 moving blips cost no layout pass.

pub mod lobby_list;
pub mod markers;
pub mod math;
pub mod radar;
pub mod scoreboard;

use regia_app::prelude::*;

use crate::color::UiColor;
use crate::focus::Focusable;
use crate::node::{Classes, Interactable, NodeName, NodeStyle, UiNode};
use crate::style::{Align, Display, Edges, JustifyContent, Style};
use crate::text::{Text, TextSpan, TextStyle, WrapMode};
use crate::theme::{ThemeColors, Themed, UiThemes};
use crate::widgets::Button;

pub use lobby_list::{
    CurrentLobby, LobbyList, LobbyListAction, LobbyListDriver, LobbyListLabels, LobbyListRequest,
    LobbyListStatus, LobbyMemberRow, LobbyRow, spawn_lobby_list,
};
pub use markers::{
    BracketReadout, MarkerLayer, MarkerLayerState, MarkerReadout, TargetLock, WorldMarker, WorldPlate,
    WorldPlateRoot, spawn_marker_layer,
};
pub use math::{LeadSolution, RadarFrame, RadarPoint, ScreenAnchor, ViewProjection};
pub use radar::{
    BlipShape, Radar, RadarBlip, RadarBlipReadout, RadarFog, RadarFogLook, RadarMap, RadarMode, RadarShape,
    RadarState, radar_fog_rgba, spawn_radar,
};
pub use scoreboard::{
    ScoreColumn, ScoreLine, ScoreRow, ScoreTable, ScoreTeam, ScoreValue, Scoreboard, ScoreboardState,
    score_lines, spawn_scoreboard,
};

/// Marks the entity the game widgets center on when they name none: a [`Radar`] without `follow`
/// and a [`TargetLock`] without `shooter` use the one with the lowest entity bits. Put it on the
/// local player's pawn. It is the designer's way to point a `.ui.ron` radar at the player.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HudFocus;

/// Reads a localization key out of a label: `"loc:menu-start"` is the key `menu-start`, anything
/// else is literal text.
pub(crate) fn label_span(label: &str, style: TextStyle) -> TextSpan {
    match label.strip_prefix("loc:") {
        Some(key) => TextSpan::loc(key),
        None => TextSpan::lit(label),
    }
    .with_style(style)
}

pub(crate) fn text_style(px: f32, color: UiColor, bold: bool) -> TextStyle {
    TextStyle {
        font_px: px,
        color,
        bold,
        ..Default::default()
    }
}

/// One line of text that never wraps.
pub(crate) fn line(label: &str, px: f32, color: UiColor, bold: bool) -> Text {
    Text {
        spans: vec![label_span(label, text_style(px, color, bold))],
        wrap: WrapMode::None,
        ..Default::default()
    }
}

/// One line of text from game or network data: never read as a localization key, so a player named
/// `loc:menu-start` shows as that.
pub(crate) fn literal_line(label: &str, px: f32, color: UiColor, bold: bool) -> Text {
    Text {
        spans: vec![TextSpan::lit(label).with_style(text_style(px, color, bold))],
        wrap: WrapMode::None,
        ..Default::default()
    }
}

/// Writes `value` only when it differs, so an idle widget wakes neither layout nor damage.
pub(crate) fn set_if_changed<C: Component<Mutability = regia_ecs::ecs::component::Mutable> + PartialEq>(
    world: &mut World,
    entity: Entity,
    value: C,
) {
    let Ok(mut e) = world.get_entity_mut(entity) else {
        return;
    };
    match e.get_mut::<C>() {
        Some(mut current) => {
            if *current != value {
                *current = value;
            }
        }
        None => {
            e.insert(value);
        }
    }
}

/// Moves a widget's state out of its node for the length of an update (a default one the first
/// time), without an archetype move per frame.
pub(crate) fn take_state<S: Component<Mutability = regia_ecs::ecs::component::Mutable> + Default>(
    world: &mut World,
    entity: Entity,
) -> S {
    match world.get_mut::<S>(entity) {
        Some(mut state) => std::mem::take(&mut *state),
        None => S::default(),
    }
}

/// Puts back what [`take_state`] took.
pub(crate) fn put_state<S: Component<Mutability = regia_ecs::ecs::component::Mutable>>(
    world: &mut World,
    entity: Entity,
    state: S,
) {
    let Ok(mut e) = world.get_entity_mut(entity) else {
        return;
    };
    match e.get_mut::<S>() {
        Some(mut current) => *current = state,
        None => {
            e.insert(state);
        }
    }
}

/// Shows or hides a node without touching the rest of its style. A node with no style yet gets one
/// to hide by.
pub(crate) fn set_shown(world: &mut World, entity: Entity, shown: bool) {
    let want = if shown { Display::Flex } else { Display::None };
    let Ok(mut e) = world.get_entity_mut(entity) else {
        return;
    };
    match e.get_mut::<NodeStyle>() {
        Some(mut style) => {
            if style.0.display != want {
                style.0.display = want;
            }
        }
        None if !shown => {
            e.insert(NodeStyle::new(Style {
                display: want,
                ..Default::default()
            }));
        }
        None => {}
    }
}

/// A plain node under `parent`.
pub(crate) fn spawn_child(world: &mut World, parent: Entity, bundle: impl Bundle) -> Entity {
    let child = world.spawn((UiNode, bundle)).id();
    world.entity_mut(parent).add_child(child);
    child
}

/// A themed, focusable button under `parent`, named `name`, with one text label.
pub(crate) fn spawn_button(
    world: &mut World,
    parent: Entity,
    name: String,
    label: &str,
    colors: &ThemeColors,
    extra: impl Bundle,
) -> (Entity, Entity) {
    let button = spawn_child(
        world,
        parent,
        (
            NodeName(name),
            NodeStyle::new(Style {
                padding: Edges::px(8.0),
                justify_content: JustifyContent::Center,
                align_items: Align::Center,
                ..Default::default()
            }),
            Interactable::default(),
            Focusable::default(),
            Button,
            Classes(vec!["button".to_owned()]),
            Themed::default(),
            extra,
        ),
    );
    let text = spawn_child(world, button, line(label, 14.0, colors.text, false));
    (button, text)
}

pub(crate) fn theme_colors(world: &World) -> ThemeColors {
    world
        .get_resource::<UiThemes>()
        .map(|t| t.current().colors.clone())
        .unwrap_or_default()
}

/// Fills every game widget in `world` with invented rows, so a designer previewing a layout sees
/// what the widget will look like in a match. The UI designer's preview calls this
/// (`regia_editor::ui_designer_preview`); nothing in a game should. Widgets that already have
/// data keep it.
pub fn fill_preview_samples(world: &mut World) {
    lobby_list::fill_preview(world);
    scoreboard::fill_preview(world);
    radar::fill_preview(world);
}

/// Adds the widgets' messages and systems to the UI plugin. Every system runs only while its widget
/// exists, so a game that uses none of them pays nothing.
pub(crate) fn install(app: &mut App) {
    app.add_message::<LobbyListRequest>()
        .init_resource::<lobby_list::LobbyListEvents>()
        .init_resource::<scoreboard::ScoreboardEvents>()
        .add_systems(
            Update,
            (
                lobby_list::lobby_list_clicks.run_if(any_with_component::<LobbyList>),
                scoreboard::scoreboard_clicks.run_if(any_with_component::<Scoreboard>),
            )
                .after(crate::interaction::ui_interaction),
        )
        .add_systems(
            PostUpdate,
            (
                lobby_list::lobby_list_update.run_if(any_with_component::<LobbyList>),
                scoreboard::scoreboard_update.run_if(any_with_component::<Scoreboard>),
                radar::radar_update.run_if(any_with_component::<Radar>),
                markers::world_plates_update
                    .run_if(any_with_component::<WorldMarker>.or_else(any_with_component::<WorldPlateRoot>)),
                markers::marker_layers_update.run_if(any_with_component::<MarkerLayer>),
            )
                .chain()
                .before(crate::layout::mark_layout_dirty)
                .in_set(PostUpdateSet::UiLayout),
        );
}
