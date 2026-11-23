// SPDX-License-Identifier: MIT-0
//! The client's HUD (feature `client`, ADR-2958): the game widgets of `regia::ui::game_widgets` on
//! this game's data.
//!
//! - A scoreboard while Tab is held: [`TagRules`] reads the engine's replicated
//!   [`PlayerScoreView`]s (kills are tags, deaths are times tagged).
//! - A minimap of the arena and a radar around the player: every avatar carries a `RadarBlip`.
//! - A marker over every other player, pinned to the screen edge when off screen, a target bracket
//!   with a lead pip on the nearest one, and a name plate in the world over the player's own avatar.
//! - A lobby list, when the app has `regia::net::Lobbies` and is not connected yet.
//!
//! TODO(template): replace the styles, and give your projectiles' speed to [`LEAD_SPEED`].

use regia::framework::game_ui::{GameUiBridgePlugin, LobbyListBridge};
use regia::framework::rules::{
    GameRules, GameRulesResource, PlayerScoreView, RulesCtx, ScoreCell, ScoreSheet, ScoreSheetColumn,
    ScoreSheetRow, SpawnRequest,
};
use regia::net::NetOwnedLocally;
use regia::net::link::NetLink;
use regia::net::lobby::LobbyServer;
use regia::prelude::*;
use regia::ui::NodeName;
use regia::ui::game_widgets::{
    HudFocus, LobbyList, MarkerLayer, Radar, RadarBlip, Scoreboard, TargetLock, WorldMarker, WorldPlate,
    spawn_lobby_list, spawn_marker_layer, spawn_radar, spawn_scoreboard,
};
use regia::ui::style::Align;
use regia_scene::GlobalTransform;
use regia_scene::transform::Transform;

use crate::view::SLOT_COLORS;
use crate::{ARENA_HALF, Avatar, Tally};

/// The muzzle speed the lead pip assumes, in m/s. This template's shots are instant rays, so the
/// pip shows what a projectile of this speed would need.
pub const LEAD_SPEED: f32 = 40.0;
/// Radius of the radar around the player, in metres.
pub const RADAR_RANGE: f32 = 12.0;

/// The rules of the template: nothing to spawn (the server spawns avatars), and the standings.
#[derive(Default)]
pub struct TagRules;

impl GameRules for TagRules {
    fn on_player_joined(&mut self, _ctx: &mut RulesCtx, _player: Entity) -> Option<SpawnRequest> {
        None
    }

    fn scoreboard(&mut self, ctx: &mut RulesCtx) -> Option<ScoreSheet> {
        let mut avatars = ctx
            .world
            .query::<(&Avatar, Option<&PlayerScoreView>, Option<&NetOwnedLocally>)>();
        Some(ScoreSheet {
            columns: vec![
                ScoreSheetColumn::new("tags", "Tags"),
                ScoreSheetColumn::new("tagged", "Tagged"),
            ],
            rows: avatars
                .iter(ctx.world)
                .map(|(avatar, view, mine)| (avatar, Tally::of(view), mine))
                .map(|(avatar, score, mine)| ScoreSheetRow {
                    id: u64::from(avatar.slot),
                    name: player_label(avatar.slot),
                    team: None,
                    local: mine.is_some(),
                    values: vec![
                        ScoreCell::Int(i64::from(score.tags)),
                        ScoreCell::Int(i64::from(score.tagged)),
                    ],
                })
                .collect(),
            teams: Vec::new(),
        })
    }
}

/// A slot's name: `Player 1` for slot 0. In `u64`: a replicated `u32::MAX` slot from a faulty
/// server neither overflows nor wraps to `Player 0` (rc1-rev-net-k 5).
fn player_label(slot: u32) -> String {
    format!("Player {}", u64::from(slot) + 1)
}

/// The HUD. Add it to a client after [`crate::view::NetGameView`].
#[derive(Default)]
pub struct NetGameHud {
    /// Where the lobby list's "start match" sends everyone: this game's dedicated server.
    /// `None`: the app's `NetSession` hosts, or the list reports `net_not_running`.
    pub lobby_server: Option<std::net::SocketAddr>,
}

impl Plugin for NetGameHud {
    fn build(&self, app: &mut App) {
        if !app.has_plugin(std::any::type_name::<UiPlugin>()) {
            app.add_plugins(UiPlugin::code_only());
        }
        if !app.has_plugin(std::any::type_name::<GameUiBridgePlugin>()) {
            app.add_plugins(GameUiBridgePlugin);
        }
        app.insert_resource(GameRulesResource {
            rules: Box::new(TagRules),
        });
        if let Some(server) = self.lobby_server {
            app.world_mut().resource_mut::<LobbyListBridge>().server = Some(LobbyServer::Address(server));
        }
        app.add_systems(Startup, spawn_hud).add_systems(
            Update,
            (
                dress_avatars,
                lock_nearest,
                hide_lobby_list_in_game,
                show_link_banner,
            ),
        );
    }
}

/// Marks an avatar the HUD has dressed.
#[derive(Component)]
struct HudDressed;

/// An avatar the HUD has not dressed yet, once it has a place in the world.
type UndressedAvatar = (Without<HudDressed>, With<GlobalTransform>);
/// The player's own avatar, until the HUD has focused on it.
type MyUnfocusedAvatar = (With<Avatar>, With<NetOwnedLocally>, Without<HudFocus>);
/// Another player's avatar, where it is, and whether it is the target.
type OtherAvatar = (Entity, &'static Transform, Has<TargetLock>);

fn slot_color(slot: u32) -> UiColor {
    let [r, g, b] = SLOT_COLORS[slot as usize % SLOT_COLORS.len()];
    UiColor([r, g, b, 1.0])
}

fn spawn_hud(mut commands: Commands, lobbies: Option<Res<regia::net::Lobbies>>) {
    let root = commands
        .spawn((
            UiRoot {
                scale_mode: ScaleMode::ScaleWithScreen {
                    reference: [1280.0, 720.0],
                    match_width: 0.5,
                },
                ..Default::default()
            },
            NodeStyle::new(Style {
                size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                ..Default::default()
            }),
            PickThrough,
        ))
        .id();
    spawn_marker_layer(&mut commands, root, MarkerLayer::default());
    let corners = commands
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                position: PositionType::Absolute,
                inset: Edges {
                    left: Dimension::Points(0.0),
                    right: Dimension::Points(0.0),
                    bottom: Dimension::Points(0.0),
                    ..Edges::default()
                },
                justify_content: JustifyContent::SpaceBetween,
                align_items: Align::End,
                padding: Edges::px(12.0),
                ..Default::default()
            }),
            PickThrough,
        ))
        .id();
    commands.entity(root).add_child(corners);
    let edge = ARENA_HALF + 1.0;
    let map = Radar::minimap([-edge, -edge, edge, edge]);
    let minimap = spawn_radar(&mut commands, corners, map, [150.0, 150.0]);
    commands.entity(minimap).insert((
        NodeName("minimap".to_owned()),
        Background {
            kind: regia::ui::BackgroundKind::Color(UiColor::from_hex("#0c1117b0").unwrap_or(UiColor::BLACK)),
        },
    ));
    let radar = spawn_radar(
        &mut commands,
        corners,
        Radar::with_range(RADAR_RANGE),
        [150.0, 150.0],
    );
    commands.entity(radar).insert((
        NodeName("radar".to_owned()),
        Background {
            kind: regia::ui::BackgroundKind::Color(UiColor::from_hex("#0c1117b0").unwrap_or(UiColor::BLACK)),
        },
        Border {
            radius: [75.0; 4],
            ..Default::default()
        },
    ));
    // The scoreboard, centered, while Tab is held.
    let middle = commands
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                position: PositionType::Absolute,
                inset: Edges {
                    left: Dimension::Points(0.0),
                    right: Dimension::Points(0.0),
                    top: Dimension::Points(48.0),
                    ..Edges::default()
                },
                justify_content: JustifyContent::Center,
                ..Default::default()
            }),
            PickThrough,
        ))
        .id();
    commands.entity(root).add_child(middle);
    // The reconnecting overlay (spec 120 § 10.2, ADR-8352): hidden while the link is up.
    let banner = commands
        .spawn((
            UiNode,
            NodeName("link_banner".to_owned()),
            LinkBannerNode,
            NodeStyle::new(Style {
                position: PositionType::Absolute,
                inset: Edges {
                    left: Dimension::Points(0.0),
                    right: Dimension::Points(0.0),
                    top: Dimension::Points(12.0),
                    ..Edges::default()
                },
                justify_content: JustifyContent::Center,
                display: Display::None,
                ..Default::default()
            }),
            regia::ui::Text::new(""),
            PickThrough,
        ))
        .id();
    commands.entity(root).add_child(banner);
    let board = spawn_scoreboard(
        &mut commands,
        middle,
        Scoreboard {
            title: "Scores".to_owned(),
            ..Scoreboard::held("Scores")
        },
    );
    commands.entity(board).insert(NodeStyle::new(Style {
        size: [Dimension::Points(420.0), Dimension::Auto],
        ..Default::default()
    }));
    // The lobby list needs lobbies; a game without any never sees it.
    if lobbies.is_some() {
        let list = spawn_lobby_list(&mut commands, middle, LobbyList::default());
        commands.entity(list).insert(NodeStyle::new(Style {
            size: [Dimension::Points(520.0), Dimension::Points(360.0)],
            ..Default::default()
        }));
    }
}

/// Gives each new avatar its blip, and every other player's a marker. Whose avatar is whose can
/// arrive after the avatar does, so the focus and the plate are checked every frame.
fn dress_avatars(
    mut commands: Commands,
    new: Query<(Entity, &Avatar), UndressedAvatar>,
    mine: Query<Entity, MyUnfocusedAvatar>,
) {
    for (entity, avatar) in &new {
        commands.entity(entity).insert((
            HudDressed,
            RadarBlip {
                label: player_label(avatar.slot),
                clamp_to_edge: true,
                ..RadarBlip::dot(slot_color(avatar.slot))
            },
            WorldMarker {
                label: player_label(avatar.slot),
                color: slot_color(avatar.slot),
                offset: [0.0, 2.4, 0.0],
                clamp_to_edge: true,
                show_distance: true,
                ..WorldMarker::default()
            },
        ));
    }
    for entity in &mine {
        commands.entity(entity).insert((
            HudFocus,
            // The player's own marker is a plate in the world: the arena can hide it.
            WorldMarker {
                label: "You".to_owned(),
                offset: [0.0, 2.4, 0.0],
                plate: Some(WorldPlate::default()),
                ..WorldMarker::default()
            },
        ));
    }
}

/// The nearest other player is the target: the bracket moves to it.
fn lock_nearest(
    mut commands: Commands,
    mine: Query<&Transform, With<HudFocus>>,
    others: Query<OtherAvatar, (With<Avatar>, Without<HudFocus>)>,
) {
    let Some(here) = mine.iter().next().map(|t| t.translation) else {
        return;
    };
    let nearest = others
        .iter()
        .min_by(|a, b| {
            a.1.translation
                .distance_squared(here)
                .total_cmp(&b.1.translation.distance_squared(here))
                .then(a.0.to_bits().cmp(&b.0.to_bits()))
        })
        .map(|(e, ..)| e);
    for (entity, _, locked) in &others {
        if Some(entity) == nearest && !locked {
            commands.entity(entity).insert(TargetLock {
                projectile_speed: Some(LEAD_SPEED),
                ..TargetLock::default()
            });
        } else if Some(entity) != nearest && locked {
            commands.entity(entity).remove::<TargetLock>();
        }
    }
}

/// The reconnecting overlay's node.
#[derive(Component)]
struct LinkBannerNode;

/// Shows the overlay while the link is interrupted, reconnecting or lost, with its line.
fn show_link_banner(
    banner: Option<Res<crate::LinkBanner>>,
    mut nodes: Query<(&mut NodeStyle, &mut regia::ui::Text), With<LinkBannerNode>>,
) {
    let line = banner.and_then(|b| b.text());
    let want = if line.is_some() {
        Display::Flex
    } else {
        Display::None
    };
    for (mut style, mut text) in &mut nodes {
        if style.0.display != want {
            style.0.display = want;
        }
        if let Some(line) = &line {
            let shown = regia::ui::Text::new(line.as_str());
            if *text != shown {
                *text = shown;
            }
        }
    }
}

/// The list is the way in: once the link is up it has done its job.
fn hide_lobby_list_in_game(link: Option<Res<NetLink>>, mut lists: Query<&mut NodeStyle, With<LobbyList>>) {
    let idle = link.is_none_or(|l| l.role() == "idle");
    let want = if idle { Display::Flex } else { Display::None };
    for mut style in &mut lists {
        if style.0.display != want {
            style.0.display = want;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Avatar;

    /// rc1-rev-net-k 5: the scoreboard of an avatar at slot `u32::MAX` names it, no overflow.
    #[test]
    fn the_last_slot_has_a_name() {
        let mut world = World::new();
        world.spawn(Avatar { slot: u32::MAX });
        world.spawn(Avatar { slot: 0 });
        let sheet = TagRules
            .scoreboard(&mut RulesCtx::new(&mut world))
            .expect("a sheet");
        let mut names: Vec<String> = sheet.rows.iter().map(|row| row.name.clone()).collect();
        names.sort();
        assert_eq!(names, ["Player 1", "Player 4294967296"]);
    }
}
