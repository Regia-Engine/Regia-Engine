// SPDX-License-Identifier: MPL-2.0
//! The lobby list (ADR-2958). A [`LobbyList`] node shows the lobbies a game can join, and, once in
//! one, its members with their ready flags. It is fed plain rows and asks for things by message:
//! its buttons send [`LobbyListRequest`]s. `regia_framework::game_ui` connects the two to
//! `regia_net::Lobbies` (the same state `lobby.*` drives); a game with its own server browser writes
//! the rows and reads the requests itself.

use regia_app::prelude::*;
use regia_ecs::ecs::message::MessageCursor;

use super::{
    line, literal_line, put_state, set_if_changed, set_shown, spawn_button, spawn_child, take_state,
    theme_colors,
};
use crate::interaction::{UiEvent, UiEventKind};
use crate::node::{Background, BackgroundKind, Border, Disabled, NodeName, NodeStyle, PickThrough};
use crate::scroll::{ScrollOffset, ScrollView};
use crate::style::{Align, Dimension, Edges, FlexDirection, JustifyContent, Style};
use crate::theme::ThemeColors;

/// Most lobby rows or member rows a list draws.
pub const MAX_LOBBY_ROWS: usize = 64;

/// One lobby that can be joined.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LobbyRow {
    /// Lobby id.
    pub id: u64,
    /// Display name.
    pub name: String,
    /// Extra text (a map, a mode).
    pub detail: String,
    /// Members in it.
    pub players: u32,
    /// Member limit.
    pub max_players: u32,
    /// Whether it can be joined (a full or started lobby is shown disabled).
    pub joinable: bool,
}

/// One member of the current lobby.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LobbyMemberRow {
    /// Member id.
    pub id: u64,
    /// Display name.
    pub name: String,
    /// Has marked ready.
    pub ready: bool,
    /// Owns the lobby.
    pub owner: bool,
    /// This machine's player.
    pub local: bool,
}

/// The lobby this machine is in.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CurrentLobby {
    /// Lobby id.
    pub id: u64,
    /// Display name.
    pub name: String,
    /// Member limit.
    pub max_players: u32,
    /// Members in join order.
    pub members: Vec<LobbyMemberRow>,
    /// This machine owns it.
    pub is_owner: bool,
    /// The owner may launch: enough members, all ready.
    pub can_launch: bool,
    /// The lobby has launched.
    pub launched: bool,
}

/// What the list reports under its heading.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum LobbyListStatus {
    /// Nothing to say.
    #[default]
    Idle,
    /// A list or join is on its way.
    Busy,
    /// The last request failed with this stable code (`lobby_full`, `lobby_not_found`, ...).
    Error(String),
}

/// Words the list draws (`"loc:<key>"` for a localization key).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LobbyListLabels {
    /// Heading.
    pub title: String,
    /// Asks for a fresh list.
    pub refresh: String,
    /// Creates a lobby.
    pub create: String,
    /// Shown while no lobby is listed.
    pub empty: String,
    /// Shown while a request is on its way.
    pub busy: String,
    /// Marks ready.
    pub ready: String,
    /// Takes the ready mark back.
    pub unready: String,
    /// Leaves the lobby.
    pub leave: String,
    /// Owner: starts the match.
    pub launch: String,
    /// Beside a member that is ready.
    pub member_ready: String,
    /// Beside the owner.
    pub member_owner: String,
}

impl Default for LobbyListLabels {
    fn default() -> Self {
        Self {
            title: "Lobbies".to_owned(),
            refresh: "Refresh".to_owned(),
            create: "Create".to_owned(),
            empty: "No lobbies found".to_owned(),
            busy: "Searching...".to_owned(),
            ready: "Ready".to_owned(),
            unready: "Not ready".to_owned(),
            leave: "Leave".to_owned(),
            launch: "Start match".to_owned(),
            member_ready: "ready".to_owned(),
            member_owner: "host".to_owned(),
        }
    }
}

/// A lobby list. Put it on a sized node, or spawn it with [`spawn_lobby_list`].
#[derive(Component, Debug, Clone, PartialEq, Default)]
pub struct LobbyList {
    /// Lobbies to show.
    pub rows: Vec<LobbyRow>,
    /// The lobby this machine is in; while set, the list shows it instead of [`Self::rows`].
    pub current: Option<CurrentLobby>,
    /// What to report.
    pub status: LobbyListStatus,
    /// The words it draws.
    pub labels: LobbyListLabels,
    /// The game fills these fields and answers the requests itself, and the driver
    /// (`regia_framework::game_ui`) leaves this list alone. Off by default: the driver's rows and
    /// state win.
    pub manual: bool,
}

/// What a lobby list asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LobbyListAction {
    /// A fresh list of lobbies.
    Refresh,
    /// A new lobby.
    Create,
    /// Join this lobby.
    Join(u64),
    /// Leave the current lobby.
    Leave,
    /// Mark the local member (not) ready.
    SetReady(bool),
    /// Owner: launch the match.
    Launch,
}

impl LobbyListAction {
    /// The action's name, as `ui.lobby_list` spells it.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Refresh => "refresh",
            Self::Create => "create",
            Self::Join(_) => "join",
            Self::Leave => "leave",
            Self::SetReady(true) => "ready",
            Self::SetReady(false) => "unready",
            Self::Launch => "launch",
        }
    }
}

/// Sent by a list's buttons and by `ui.lobby_list`.
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct LobbyListRequest {
    /// The list.
    pub list: Entity,
    /// What it asks for.
    pub action: LobbyListAction,
}

/// Present while something answers [`LobbyListRequest`]s: `regia_framework::game_ui`, or a game's
/// own server browser. `ui.lobby_list` refuses actions without it.
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct LobbyListDriver {
    /// Who answers (`"regia_net"`).
    pub name: String,
}

/// A button of a list.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct LobbyListButton {
    /// The list.
    pub list: Entity,
    /// What it asks for.
    pub action: LobbyListAction,
}

#[derive(Resource, Default)]
pub(crate) struct LobbyListEvents(MessageCursor<UiEvent>);

/// Runtime state of a [`LobbyList`], kept on its node.
#[derive(Component, Debug, Clone, Default, PartialEq, Eq)]
pub struct LobbyListNodes {
    title: Option<Entity>,
    status: Option<Entity>,
    refresh: Option<(Entity, Entity)>,
    create: Option<(Entity, Entity)>,
    list_body: Option<Entity>,
    rows: Vec<LobbyRowNodes>,
    lobby_body: Option<Entity>,
    lobby_title: Option<Entity>,
    members: Vec<MemberNodes>,
    ready: Option<(Entity, Entity)>,
    launch: Option<(Entity, Entity)>,
    leave: Option<(Entity, Entity)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LobbyRowNodes {
    node: Entity,
    name: Entity,
    detail: Entity,
    players: Entity,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MemberNodes {
    node: Entity,
    name: Entity,
    state: Entity,
}

/// Spawns `list` on a new node under `parent`.
pub fn spawn_lobby_list(commands: &mut Commands, parent: Entity, list: LobbyList) -> Entity {
    let node = commands
        .spawn((
            crate::node::UiNode,
            NodeName("lobby_list".to_owned()),
            NodeStyle::default(),
            list,
        ))
        .id();
    commands.entity(parent).add_child(node);
    node
}

/// `Update`: a click on a list's button sends its request.
pub(crate) fn lobby_list_clicks(world: &mut World) {
    let mut cursor = world.remove_resource::<LobbyListEvents>().unwrap_or_default();
    let clicked: Vec<Entity> = world
        .get_resource::<Messages<UiEvent>>()
        .map(|m| {
            cursor
                .0
                .read(m)
                .filter(|e| matches!(e.kind, UiEventKind::Click))
                .map(|e| e.target)
                .collect()
        })
        .unwrap_or_default();
    world.insert_resource(cursor);
    for target in clicked {
        let Some(button) = world.get::<LobbyListButton>(target).copied() else {
            continue;
        };
        if crate::node::is_disabled(world, target) {
            continue;
        }
        world.write_message(LobbyListRequest {
            list: button.list,
            action: button.action,
        });
    }
}

fn row_node_style() -> Style {
    Style {
        direction: FlexDirection::Row,
        align_items: Align::Center,
        justify_content: JustifyContent::SpaceBetween,
        gap: [Dimension::Points(12.0), Dimension::Points(12.0)],
        padding: Edges::px(8.0),
        size: [Dimension::Percent(100.0), Dimension::Auto],
        ..Default::default()
    }
}

fn grow() -> NodeStyle {
    NodeStyle::new(Style {
        flex_grow: 1.0,
        ..Default::default()
    })
}

fn build(
    world: &mut World,
    entity: Entity,
    labels: &LobbyListLabels,
    colors: &ThemeColors,
) -> LobbyListNodes {
    if world.get::<Background>(entity).is_none() {
        world.entity_mut(entity).insert((
            Background {
                kind: BackgroundKind::Color(colors.surface.with_alpha(0.92)),
            },
            Border {
                color: colors.border,
                widths: [1.0; 4],
                radius: [6.0; 4],
            },
        ));
    }
    if let Some(mut style) = world.get_mut::<NodeStyle>(entity) {
        style.0.direction = FlexDirection::Column;
        style.0.gap = [Dimension::Points(8.0), Dimension::Points(8.0)];
        style.0.padding = Edges::px(12.0);
    }
    let header = spawn_child(
        world,
        entity,
        NodeStyle::new(Style {
            direction: FlexDirection::Row,
            align_items: Align::Center,
            gap: [Dimension::Points(8.0), Dimension::Points(8.0)],
            ..Default::default()
        }),
    );
    let title = spawn_child(
        world,
        header,
        (grow(), line(&labels.title, 18.0, colors.text, true), PickThrough),
    );
    let refresh = spawn_button(
        world,
        header,
        "lobby_list.refresh".to_owned(),
        &labels.refresh,
        colors,
        LobbyListButton {
            list: entity,
            action: LobbyListAction::Refresh,
        },
    );
    let create = spawn_button(
        world,
        header,
        "lobby_list.create".to_owned(),
        &labels.create,
        colors,
        LobbyListButton {
            list: entity,
            action: LobbyListAction::Create,
        },
    );
    let status = spawn_child(
        world,
        entity,
        (
            NodeName("lobby_list.status".to_owned()),
            line("", 13.0, colors.text_dim, false),
            PickThrough,
        ),
    );
    let list_body = spawn_child(
        world,
        entity,
        (
            NodeName("lobby_list.rows".to_owned()),
            NodeStyle::new(Style {
                direction: FlexDirection::Column,
                gap: [Dimension::Points(4.0), Dimension::Points(4.0)],
                flex_grow: 1.0,
                ..Default::default()
            }),
            crate::node::ClipChildren,
            crate::node::Interactable::default(),
            ScrollView::default(),
            ScrollOffset::default(),
        ),
    );
    let lobby_body = spawn_child(
        world,
        entity,
        (
            NodeName("lobby_list.lobby".to_owned()),
            NodeStyle::new(Style {
                direction: FlexDirection::Column,
                gap: [Dimension::Points(4.0), Dimension::Points(4.0)],
                flex_grow: 1.0,
                ..Default::default()
            }),
        ),
    );
    let lobby_title = spawn_child(
        world,
        lobby_body,
        (line("", 16.0, colors.text, true), PickThrough),
    );
    // The buttons sit above the members, so they stay put as members come and go.
    let buttons = spawn_child(
        world,
        lobby_body,
        NodeStyle::new(Style {
            direction: FlexDirection::Row,
            gap: [Dimension::Points(8.0), Dimension::Points(8.0)],
            ..Default::default()
        }),
    );
    let ready = spawn_button(
        world,
        buttons,
        "lobby_list.ready".to_owned(),
        &labels.ready,
        colors,
        LobbyListButton {
            list: entity,
            action: LobbyListAction::SetReady(true),
        },
    );
    let launch = spawn_button(
        world,
        buttons,
        "lobby_list.launch".to_owned(),
        &labels.launch,
        colors,
        LobbyListButton {
            list: entity,
            action: LobbyListAction::Launch,
        },
    );
    let leave = spawn_button(
        world,
        buttons,
        "lobby_list.leave".to_owned(),
        &labels.leave,
        colors,
        LobbyListButton {
            list: entity,
            action: LobbyListAction::Leave,
        },
    );
    LobbyListNodes {
        title: Some(title),
        status: Some(status),
        refresh: Some(refresh),
        create: Some(create),
        list_body: Some(list_body),
        rows: Vec::new(),
        lobby_body: Some(lobby_body),
        lobby_title: Some(lobby_title),
        members: Vec::new(),
        ready: Some(ready),
        launch: Some(launch),
        leave: Some(leave),
    }
}

fn spawn_lobby_row(
    world: &mut World,
    entity: Entity,
    body: Entity,
    colors: &ThemeColors,
    i: usize,
) -> LobbyRowNodes {
    let (node, placeholder) = spawn_button(
        world,
        body,
        format!("lobby_list.row.{i}"),
        "",
        colors,
        LobbyListButton {
            list: entity,
            action: LobbyListAction::Refresh,
        },
    );
    if let Ok(e) = world.get_entity_mut(placeholder) {
        e.despawn();
    }
    if let Some(mut style) = world.get_mut::<NodeStyle>(node) {
        style.0 = row_node_style();
    }
    let name = spawn_child(
        world,
        node,
        (grow(), line("", 14.0, colors.text, true), PickThrough),
    );
    let detail = spawn_child(world, node, (line("", 13.0, colors.text_dim, false), PickThrough));
    let players = spawn_child(world, node, (line("", 13.0, colors.text, false), PickThrough));
    LobbyRowNodes {
        node,
        name,
        detail,
        players,
    }
}

fn spawn_member_row(world: &mut World, body: Entity, colors: &ThemeColors, i: usize) -> MemberNodes {
    let node = spawn_child(
        world,
        body,
        (
            NodeName(format!("lobby_list.member.{i}")),
            NodeStyle::new(row_node_style()),
        ),
    );
    let name = spawn_child(
        world,
        node,
        (grow(), line("", 14.0, colors.text, false), PickThrough),
    );
    let state = spawn_child(world, node, (line("", 13.0, colors.text_dim, false), PickThrough));
    MemberNodes { node, name, state }
}

fn set_label(
    world: &mut World,
    text: Entity,
    label: &str,
    px: f32,
    color: crate::color::UiColor,
    bold: bool,
) {
    set_if_changed(world, text, line(label, px, color, bold));
}

/// [`set_label`] for a lobby's or a player's own words.
fn set_data(world: &mut World, text: Entity, data: &str, px: f32, color: crate::color::UiColor, bold: bool) {
    set_if_changed(world, text, literal_line(data, px, color, bold));
}

/// `PostUpdate`, before layout: redraws each list from its fields.
pub(crate) fn lobby_list_update(world: &mut World) {
    let lists: Vec<Entity> = world
        .query_filtered::<Entity, With<LobbyList>>()
        .iter(world)
        .collect();
    if lists.is_empty() {
        return;
    }
    let _span = tracing::trace_span!("ui_lobby_list").entered();
    let colors = theme_colors(world);
    for entity in lists {
        let Some(list) = world.get::<LobbyList>(entity).cloned() else {
            continue;
        };
        let mut nodes: LobbyListNodes = take_state(world, entity);
        if nodes.title.is_none() {
            nodes = build(world, entity, &list.labels, &colors);
        }
        refresh(world, entity, &list, &mut nodes, &colors);
        put_state(world, entity, nodes);
    }
}

fn refresh(
    world: &mut World,
    entity: Entity,
    list: &LobbyList,
    nodes: &mut LobbyListNodes,
    colors: &ThemeColors,
) {
    let labels = &list.labels;
    let in_lobby = list.current.is_some();
    if let Some(title) = nodes.title {
        set_label(world, title, &labels.title, 18.0, colors.text, true);
    }
    if let Some((b, text)) = nodes.refresh {
        set_shown(world, b, !in_lobby);
        set_label(world, text, &labels.refresh, 14.0, colors.text, false);
    }
    if let Some((b, text)) = nodes.create {
        set_shown(world, b, !in_lobby);
        set_label(world, text, &labels.create, 14.0, colors.text, false);
    }
    // The status line.
    let (status, tone) = match (&list.status, in_lobby, list.rows.is_empty()) {
        (LobbyListStatus::Error(code), ..) => (code.clone(), colors.danger),
        (LobbyListStatus::Busy, ..) => (labels.busy.clone(), colors.text_dim),
        (LobbyListStatus::Idle, false, true) => (labels.empty.clone(), colors.text_dim),
        _ => (String::new(), colors.text_dim),
    };
    if let Some(node) = nodes.status {
        set_shown(world, node, !status.is_empty());
        set_label(world, node, &status, 13.0, tone, false);
    }
    if let Some(body) = nodes.list_body {
        set_shown(world, body, !in_lobby);
        refresh_rows(world, entity, list, nodes, body, colors);
    }
    if let Some(body) = nodes.lobby_body {
        set_shown(world, body, in_lobby);
        if let Some(lobby) = &list.current {
            refresh_lobby(world, entity, lobby, labels, nodes, body, colors);
        }
    }
}

fn refresh_rows(
    world: &mut World,
    entity: Entity,
    list: &LobbyList,
    nodes: &mut LobbyListNodes,
    body: Entity,
    colors: &ThemeColors,
) {
    let shown = list.rows.len().min(MAX_LOBBY_ROWS);
    while nodes.rows.len() < shown {
        let i = nodes.rows.len();
        let row = spawn_lobby_row(world, entity, body, colors, i);
        nodes.rows.push(row);
    }
    for (row, slot) in list.rows.iter().take(shown).zip(nodes.rows.iter()) {
        set_shown(world, slot.node, true);
        set_if_changed(
            world,
            slot.node,
            LobbyListButton {
                list: entity,
                action: LobbyListAction::Join(row.id),
            },
        );
        let name = if row.name.is_empty() {
            format!("Lobby {}", row.id)
        } else {
            row.name.clone()
        };
        set_data(world, slot.name, &name, 14.0, colors.text, true);
        set_data(world, slot.detail, &row.detail, 13.0, colors.text_dim, false);
        set_label(
            world,
            slot.players,
            &format!("{}/{}", row.players, row.max_players),
            13.0,
            colors.text,
            false,
        );
        let disabled = world.get::<Disabled>(slot.node).is_some();
        if disabled == row.joinable {
            if row.joinable {
                world.entity_mut(slot.node).remove::<Disabled>();
            } else {
                world.entity_mut(slot.node).insert(Disabled);
            }
        }
    }
    for extra in nodes.rows.iter().skip(shown) {
        set_shown(world, extra.node, false);
    }
}

fn refresh_lobby(
    world: &mut World,
    entity: Entity,
    lobby: &CurrentLobby,
    labels: &LobbyListLabels,
    nodes: &mut LobbyListNodes,
    body: Entity,
    colors: &ThemeColors,
) {
    if let Some(title) = nodes.lobby_title {
        let name = if lobby.name.is_empty() {
            format!("Lobby {}", lobby.id)
        } else {
            lobby.name.clone()
        };
        let text = format!("{name}  {}/{}", lobby.members.len(), lobby.max_players);
        set_data(world, title, &text, 16.0, colors.text, true);
    }
    let local_ready = lobby.members.iter().any(|m| m.local && m.ready);
    if let Some((b, text)) = nodes.ready {
        set_if_changed(
            world,
            b,
            LobbyListButton {
                list: entity,
                action: LobbyListAction::SetReady(!local_ready),
            },
        );
        let label = if local_ready {
            &labels.unready
        } else {
            &labels.ready
        };
        set_label(world, text, label, 14.0, colors.text, false);
    }
    if let Some((b, text)) = nodes.launch {
        set_shown(world, b, lobby.is_owner);
        set_label(world, text, &labels.launch, 14.0, colors.text, false);
        let disabled = world.get::<Disabled>(b).is_some();
        if disabled == lobby.can_launch {
            if lobby.can_launch {
                world.entity_mut(b).remove::<Disabled>();
            } else {
                world.entity_mut(b).insert(Disabled);
            }
        }
    }
    if let Some((_, text)) = nodes.leave {
        set_label(world, text, &labels.leave, 14.0, colors.text, false);
    }
    let shown = lobby.members.len().min(MAX_LOBBY_ROWS);
    while nodes.members.len() < shown {
        let i = nodes.members.len();
        let row = spawn_member_row(world, body, colors, i);
        nodes.members.push(row);
    }
    for (member, slot) in lobby.members.iter().take(shown).zip(nodes.members.iter()) {
        set_shown(world, slot.node, true);
        let tone = if member.local { colors.primary } else { colors.text };
        let name = if member.name.is_empty() {
            format!("Player {}", member.id)
        } else {
            member.name.clone()
        };
        set_data(world, slot.name, &name, 14.0, tone, member.local);
        let mut state: Vec<&str> = Vec::new();
        if member.owner {
            state.push(&labels.member_owner);
        }
        if member.ready {
            state.push(&labels.member_ready);
        }
        set_label(world, slot.state, &state.join("  "), 13.0, colors.text_dim, false);
    }
    for extra in nodes.members.iter().skip(shown) {
        set_shown(world, extra.node, false);
    }
}

/// Invented rows for the designer's preview, on lists with none.
pub(crate) fn fill_preview(world: &mut World) {
    let lists: Vec<Entity> = world
        .query_filtered::<Entity, With<LobbyList>>()
        .iter(world)
        .collect();
    for entity in lists {
        if let Some(mut list) = world.get_mut::<LobbyList>(entity)
            && list.rows.is_empty()
            && list.current.is_none()
        {
            list.manual = true;
            list.rows = (1..=3)
                .map(|i| LobbyRow {
                    id: i,
                    name: format!("Sample lobby {i}"),
                    detail: "Sample map".to_owned(),
                    players: i as u32 + 1,
                    max_players: 8,
                    joinable: i != 3,
                })
                .collect();
        }
    }
}
