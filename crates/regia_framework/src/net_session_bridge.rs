// SPDX-License-Identifier: MPL-2.0
//! The session ↔ framework bridge (spec 77 § 9, spec 74 § 9; ADR-2413), feature `net`.
//!
//! [`NetSessionBridgePlugin`] connects `regia_net`'s session to the framework:
//!
//! - **Game rules.** A remote player reaching `InGame` on the server calls
//!   [`GameRules::on_player_joined`](crate::rules::GameRules::on_player_joined) with its `NetPlayer`
//!   entity, once per player for the whole session (a travel or a reconnect resume reaches `InGame`
//!   again and does not call it again). A returned [`SpawnRequest`](crate::rules::SpawnRequest)
//!   spawns the character the same way a local join does, owned by the player's peer (`NetOwner`),
//!   and [`NetRulesJoined::pawn`] names it. Leaving (`Gone`, or one player kicked from a split-screen
//!   peer) calls [`GameRules::on_player_left`](crate::rules::GameRules::on_player_left) once, before
//!   the entity is despawned, and only for a player whose join was called (a peer that leaves while
//!   loading never joined). Then the peer's owned entities are handed on (`regia_net::session::players`):
//!   the character the join spawned carries [`NetDespawnWithOwner`], so it is despawned with its
//!   player (KI-GP-033, ADR-10070). Rules that keep a leaver's character remove that marker (or give
//!   the character a live `NetSuccessor`) in `on_player_left`; a takeover bot that drives the
//!   character lifts the marker while it drives.
//!   Local players keep the `PlayerManager` flow of [`crate::player`].
//! - **Remote players (spec 70 § 4, ADR-7212).** A joining peer's `NetPlayer` entity becomes a
//!   [`Player`] with [`PlayerKind::Remote`](crate::player::PlayerKind::Remote) through
//!   [`PlayerManager`] at its welcome, with a [`PlayerController`] that persists across travel; leaving
//!   frees the slot and despawns the controller (after `on_player_left`). Nothing happens without a
//!   `PlayerManager` resource.
//! - **Possession ownership (spec 70 § 5.2, ADR-7212).** `PlayerController::possess` is a plain
//!   method with no world, so [`sync_possession_owners`] (in `FixedUpdate`, after gameplay and
//!   before [`NetSet::Assign`](regia_net::NetSet::Assign), so before the replication send of the
//!   same tick) turns a remote player's controller change into `NetOwner`: the possessed pawn is
//!   owned by the player's peer, an unpossessed or replaced pawn loses the owner it got this way,
//!   and the change rides the same tick's gather instead of waiting a frame. Changes made outside
//!   the fixed tick (an `Update` system, an observer) are picked up by the next fixed tick, still
//!   before its gather. Local players and bots have no `NetPlayer` and are left alone.
//! - **No collider while hidden.** A proxy kept hidden until its spawn tick is shown
//!   (`NetHiddenUntilShown`, spec 74 § 9) has no [`Collider`] on itself or its descendants: the
//!   component waits in [`NetHiddenCollider`] and comes back when the proxy is revealed. This runs
//!   in observers, so it acts the moment the marker or a collider is added, before any physics
//!   step, and adds no system to any schedule.
//! - **Stats overlay.** With the `client` feature it adds
//!   [`NetStatsOverlayPlugin`](crate::net_stats_overlay::NetStatsOverlayPlugin).
//!
//! [`RulesPlugin`](crate::rules::RulesPlugin) adds this plugin when the `net` feature is on; add it
//! yourself in an app without `RulesPlugin` (the hooks then fall back to
//! [`DefaultRules`](crate::rules::DefaultRules), like a local join).

use regia_app::prelude::*;
use regia_net::NetPersistAcrossTravel;
use regia_net::interp::NetHiddenUntilShown;
use regia_net::model::{NetOwner, NetPlayer, NetPlayerId, PeerId, set_possession_owner};
use regia_net::server_host::{HostEvent, HostGameHooks};
use regia_net::session::{NetDespawnWithOwner, NetPlayerHooks, NetSession, SessionRules};
use regia_physics::Collider;
use regia_scene::level::LevelSettings;

use crate::player::{Player, PlayerController, PlayerManager};
use crate::rules::{GameRulesResource, MatchOptions, MatchRules, RotationModeOverride, RulesCtx};

/// Ancestors walked looking for a hidden proxy above a new collider.
const HIDDEN_WALK_LIMIT: usize = 64;

/// On a remote player's `NetPlayer` entity (server) once `GameRules::on_player_joined` ran for it.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetRulesJoined {
    /// The character spawned for the rules' `SpawnRequest` (`None` when the rules spawned nothing).
    pub pawn: Option<Entity>,
}

/// On a remote player's controller (server): the pawn its possession made owned by the player's
/// peer (spec 70 § 5.2). [`sync_possession_owners`] keeps it.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetPossessionOwner {
    /// The possessed pawn that got `NetOwner` from this controller.
    pub pawn: Entity,
    /// The peer it was given to.
    pub peer: PeerId,
}

/// A collider set aside while its proxy is hidden (spec 74 § 9).
#[derive(Component, Debug, Clone, PartialEq)]
pub struct NetHiddenCollider(pub Collider);

/// Wires the session to `GameRules` and keeps hidden proxies out of physics (see the module docs).
#[derive(Debug, Default, Clone, Copy)]
pub struct NetSessionBridgePlugin;

impl Plugin for NetSessionBridgePlugin {
    fn build(&self, app: &mut App) {
        install_rules_hooks(app.world_mut());
        app.init_resource::<HostGameHooks>();
        {
            let mut hooks = app.world_mut().resource_mut::<HostGameHooks>();
            hooks.add_event_hook(rotation_host_hook);
            // A dedicated host resolves its playlist rows through the game's rule sets and puts
            // their hash in the session manifest (ADR-8881).
            hooks.set_rule_sets(registry_rule_sets);
        }
        install_game_rules_session_rules(app.world_mut());
        app.add_systems(Update, ensure_game_rules_session_rules);
        // After gameplay (which may possess) and before the replication gather of the same tick,
        // so a possession change becomes NetOwner in the tick it was made (spec 70 § 5.2).
        app.add_systems(
            FixedUpdate,
            sync_possession_owners
                .after(regia_net::NetSet::Gameplay)
                .before(regia_net::NetSet::Assign),
        );
        app.add_observer(hide_on_marker)
            .add_observer(hide_new_collider)
            .add_observer(reveal_on_marker_removed);
        #[cfg(feature = "client")]
        if !app.has_plugin(std::any::type_name::<
            crate::net_stats_overlay::NetStatsOverlayPlugin,
        >()) {
            app.add_plugins(crate::net_stats_overlay::NetStatsOverlayPlugin);
        }
    }
}

/// Set once the bridge has considered installing [`GameRulesSessionRules`].
#[derive(Resource, Debug, Default)]
struct SessionRulesBridgeInstalled;

/// Session rules that follow the level's `GameRules` and, when the app holds `MatchRules`, the
/// match board (`book/agents/net-session-players.md`).
///
/// `match_over` is true when `MatchRules.session.board.match_over` is set, or when
/// `GameRules::is_match_over` is. `on_travel_finished` calls `GameRules::on_level_start`.
#[derive(Debug, Default, Clone, Copy)]
pub struct GameRulesSessionRules;

impl SessionRules for GameRulesSessionRules {
    fn match_over(&self, world: &World) -> bool {
        if world
            .get_resource::<MatchRules>()
            .is_some_and(|rules| rules.session.board.match_over)
        {
            return true;
        }
        world
            .get_resource::<GameRulesResource>()
            .is_some_and(|rules| rules.rules.is_match_over(world))
    }

    fn on_travel_finished(&mut self, world: &mut World, _level: &str) {
        let mut rules = world.remove_resource::<GameRulesResource>().unwrap_or_default();
        {
            let mut ctx = RulesCtx::new(world);
            rules.rules.on_level_start(&mut ctx);
        }
        world.insert_resource(rules);
    }
}

/// Installs [`GameRulesSessionRules`] when the app has a session and has not called
/// `set_session_rules`.
fn install_game_rules_session_rules(world: &mut World) {
    if world.contains_resource::<SessionRulesBridgeInstalled>() {
        return;
    }
    if !world.contains_resource::<NetSession>() {
        return;
    }
    world
        .resource_mut::<NetSession>()
        .install_rules_unless_explicit(GameRulesSessionRules);
    world.insert_resource(SessionRulesBridgeInstalled);
}

/// Retries the session-rules install until a session exists, and keeps the rotation hook.
fn ensure_game_rules_session_rules(world: &mut World) {
    install_game_rules_session_rules(world);
    let mut hooks = world.get_resource_or_insert_with(HostGameHooks::default);
    hooks.add_event_hook(rotation_host_hook);
    if hooks.rule_sets().is_none() {
        hooks.set_rule_sets(registry_rule_sets);
    }
}

/// The dedicated host's rule sets (ADR-8881): the world's `RuleSetRegistry` library (the shipped
/// defaults plus the project's `assets/rules/`), or `None` when the app has no registry.
fn registry_rule_sets(world: &World) -> Option<regia_rules::ruleset::RuleSetLibrary> {
    world
        .get_resource::<crate::ruleset_file::RuleSetRegistry>()
        .map(|registry| registry.library().clone())
}

/// Copies a playlist row into `MatchOptions`, `RotationModeOverride` and, when the mode is
/// non-empty, `LevelSettings.game_rules`. `LevelLoaded` applies the override again because
/// loading the level may replace `LevelSettings`.
fn rotation_host_hook(world: &mut World, event: &HostEvent<'_>) -> Result<(), String> {
    match event {
        HostEvent::RotationRow { mode, options, .. } => {
            let mut map = std::collections::BTreeMap::new();
            for (key, value) in options.iter() {
                map.insert((*key).to_owned(), (*value).to_owned());
            }
            world.insert_resource(MatchOptions { options: map });
            let mode = (*mode).to_owned();
            world.insert_resource(RotationModeOverride { mode: mode.clone() });
            if !mode.is_empty() {
                let mut settings = world.get_resource_or_insert_with(LevelSettings::default);
                settings.game_rules = Some(mode);
            }
            Ok(())
        }
        HostEvent::LevelLoaded { .. } => {
            let mode = world
                .get_resource::<RotationModeOverride>()
                .filter(|over| !over.mode.is_empty())
                .map(|over| over.mode.clone());
            if let Some(mode) = mode {
                let mut settings = world.get_resource_or_insert_with(LevelSettings::default);
                settings.game_rules = Some(mode);
            }
            crate::rules::apply_level_rules(world);
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Installs the `GameRules` join and leave hooks into the session's [`NetPlayerHooks`].
pub fn install_rules_hooks(world: &mut World) {
    let mut hooks = world.get_resource_or_insert_with(NetPlayerHooks::default);
    hooks.add_welcomed(remote_player_welcomed);
    hooks.add_joined(rules_player_joined);
    hooks.add_left(rules_player_left);
    hooks.add_left(remote_player_left);
}

/// The welcome hook: the joining peer's `NetPlayer` entity becomes a remote `Player` through the
/// `PlayerManager` (spec 70 § 4), its controller persisting across travel like the player entity.
pub fn remote_player_welcomed(world: &mut World, player_entity: Entity, player: NetPlayerId) {
    if !world.contains_resource::<PlayerManager>() {
        return;
    }
    let Some(controller) = PlayerManager::adopt_remote_static(world, player_entity) else {
        tracing::warn!(
            target: "regia_framework",
            peer = player.peer.0,
            local = player.local,
            "remote player not added to the PlayerManager (no free index, or already a player)"
        );
        return;
    };
    if let Ok(mut e) = world.get_entity_mut(controller) {
        e.insert(NetPersistAcrossTravel);
    }
    tracing::debug!(
        target: "regia_framework",
        peer = player.peer.0,
        local = player.local,
        controller = controller.to_bits(),
        "remote player added to the PlayerManager"
    );
}

/// The second leave hook (after `on_player_left`): frees the remote player's `PlayerManager` slot
/// and despawns its controller.
pub fn remote_player_left(world: &mut World, player_entity: Entity, _player: NetPlayerId) {
    PlayerManager::release_remote_static(world, player_entity);
}

/// `FixedUpdate`, after gameplay and before the replication send (spec 70 § 5.2): turns remote
/// players' possession changes into `NetOwner` in the same tick. A controller that possesses a
/// pawn makes its player's peer the pawn's owner; when it possesses another pawn or unpossesses,
/// the pawn it made owned loses that owner (an owner the game set to someone else since is left
/// alone). Changes made outside the fixed tick are picked up by the next one.
pub fn sync_possession_owners(world: &mut World) {
    let mut changed = world.query_filtered::<
        (Entity, &PlayerController, Option<&NetPossessionOwner>),
        Changed<PlayerController>,
    >();
    let moves: Vec<(Entity, Entity, Option<NetPossessionOwner>)> = changed
        .iter(world)
        .filter(|(_, pc, had)| had.map(|h| h.pawn) != Some(pc.possessed))
        .filter(|(_, pc, had)| had.is_some() || pc.possessed != Entity::PLACEHOLDER)
        .map(|(e, pc, had)| (e, pc.possessed, had.copied()))
        .collect();
    if moves.is_empty() {
        return;
    }
    let _span = tracing::debug_span!(target: "regia_framework", "net_possession_owners").entered();
    let mut players = world.query::<(&Player, &NetPlayer)>();
    let peer_of: Vec<(Entity, PeerId)> = players
        .iter(world)
        .map(|(p, np)| (p.controller, np.id.peer))
        .collect();
    for (controller, pawn, had) in moves {
        let Some(peer) = peer_of.iter().find(|(c, _)| *c == controller).map(|(_, p)| *p) else {
            continue;
        };
        if let Some(old) = had
            && world.get::<NetOwner>(old.pawn) == Some(&NetOwner(old.peer))
        {
            set_possession_owner(world, old.pawn, None);
        }
        if pawn == Entity::PLACEHOLDER || world.get_entity(pawn).is_err() {
            if let Ok(mut c) = world.get_entity_mut(controller) {
                c.remove::<NetPossessionOwner>();
            }
            continue;
        }
        let Ok(mut c) = world.get_entity_mut(controller) else {
            continue;
        };
        c.insert(NetPossessionOwner { pawn, peer });
        set_possession_owner(world, pawn, Some(peer));
    }
}

/// The join hook: `GameRules::on_player_joined` once per remote player, then its spawn request.
pub fn rules_player_joined(world: &mut World, player_entity: Entity, player: NetPlayerId) {
    if world.get::<NetRulesJoined>(player_entity).is_some() {
        return;
    }
    let _span = tracing::debug_span!(target: "regia_framework", "net_rules_joined").entered();
    let mut rules = world.remove_resource::<GameRulesResource>().unwrap_or_default();
    let spawn = {
        let mut ctx = RulesCtx::new(world);
        rules.rules.on_player_joined(&mut ctx, player_entity)
    };
    world.insert_resource(rules);
    let pawn = spawn.map(|req| {
        let pawn = crate::player::spawn_pawn(world, &req);
        crate::rules::claim_pawn_seat(world, player_entity, pawn);
        if let Ok(mut e) = world.get_entity_mut(pawn) {
            // KI-GP-033 (ADR-10070): the character leaves with its player unless the rules keep it.
            e.insert((NetOwner(player.peer), NetDespawnWithOwner));
        }
        pawn
    });
    if let Ok(mut e) = world.get_entity_mut(player_entity) {
        e.insert(NetRulesJoined { pawn });
    }
    tracing::info!(
        target: "regia_framework",
        peer = player.peer.0,
        local = player.local,
        pawn = pawn.map(Entity::to_bits),
        "remote player joined the game rules"
    );
}

/// The leave hook: `GameRules::on_player_left` for a player whose join hook ran.
pub fn rules_player_left(world: &mut World, player_entity: Entity, player: NetPlayerId) {
    let Some(joined) = world.get::<NetRulesJoined>(player_entity).copied() else {
        return;
    };
    let _span = tracing::debug_span!(target: "regia_framework", "net_rules_left").entered();
    if let Ok(mut e) = world.get_entity_mut(player_entity) {
        e.remove::<NetRulesJoined>();
    }
    let mut rules = world.remove_resource::<GameRulesResource>().unwrap_or_default();
    {
        let mut ctx = RulesCtx::new(world);
        rules.rules.on_player_left(&mut ctx, player_entity);
    }
    world.insert_resource(rules);
    tracing::info!(
        target: "regia_framework",
        peer = player.peer.0,
        local = player.local,
        pawn = joined.pawn.map(Entity::to_bits),
        "remote player left the game rules"
    );
}

/// The entity and its descendants (breadth first, bounded).
fn subtree(world: &World, root: Entity) -> Vec<Entity> {
    let mut out = vec![root];
    let mut i = 0;
    while let Some(&e) = out.get(i) {
        i += 1;
        if out.len() > 4096 {
            break;
        }
        if let Some(children) = world.get::<Children>(e) {
            out.extend(children.iter());
        }
    }
    out
}

/// Takes the colliders of a hidden proxy's subtree out of physics.
pub fn stash_colliders(world: &mut World, root: Entity) {
    for e in subtree(world, root) {
        let Ok(mut ent) = world.get_entity_mut(e) else {
            continue;
        };
        if let Some(c) = ent.take::<Collider>() {
            ent.insert(NetHiddenCollider(c));
        }
    }
}

/// Gives a revealed proxy's subtree its colliders back (nothing while it is still hidden).
pub fn restore_colliders(world: &mut World, root: Entity) {
    if world.get::<NetHiddenUntilShown>(root).is_some() {
        return;
    }
    for e in subtree(world, root) {
        let Ok(mut ent) = world.get_entity_mut(e) else {
            continue;
        };
        if let Some(NetHiddenCollider(c)) = ent.take::<NetHiddenCollider>() {
            ent.insert(c);
        }
    }
}

fn hide_on_marker(on: On<Add<NetHiddenUntilShown>>, mut commands: Commands) {
    let root = on.event().entity;
    commands.queue(move |world: &mut World| {
        if world.get::<NetHiddenUntilShown>(root).is_some() {
            stash_colliders(world, root);
        }
    });
}

fn hide_new_collider(
    on: On<Add<Collider>>,
    hidden: Query<(), With<NetHiddenUntilShown>>,
    parents: Query<&ChildOf>,
    mut commands: Commands,
) {
    let entity = on.event().entity;
    let mut at = entity;
    let mut under_hidden = false;
    for _ in 0..HIDDEN_WALK_LIMIT {
        if hidden.contains(at) {
            under_hidden = true;
            break;
        }
        match parents.get(at) {
            Ok(p) => at = p.parent(),
            Err(_) => break,
        }
    }
    if !under_hidden {
        return;
    }
    commands.queue(move |world: &mut World| {
        if let Ok(mut ent) = world.get_entity_mut(entity)
            && let Some(c) = ent.take::<Collider>()
        {
            ent.insert(NetHiddenCollider(c));
        }
    });
}

fn reveal_on_marker_removed(on: On<Remove<NetHiddenUntilShown>>, mut commands: Commands) {
    let root = on.event().entity;
    // Runs after the removal (and is a no-op for a despawned proxy).
    commands.queue(move |world: &mut World| {
        if world.get_entity(root).is_ok() {
            restore_colliders(world, root);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_net::model::NetTick;

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(NetSessionBridgePlugin);
        app
    }

    #[test]
    fn a_hidden_proxy_and_its_children_have_no_collider_until_shown() {
        let mut app = app();
        let world = app.world_mut();
        let root = world.spawn(Collider::default()).id();
        let child = world.spawn((Collider::default(), ChildOf(root))).id();
        world.entity_mut(root).insert(NetHiddenUntilShown {
            tick: NetTick(5),
            previous: None,
        });
        world.flush();
        assert!(world.get::<Collider>(root).is_none());
        assert!(world.get::<Collider>(child).is_none());
        assert!(world.get::<NetHiddenCollider>(child).is_some());
        // A collider added while hidden (a prefab finishing) waits too.
        let late = world.spawn((Collider::default(), ChildOf(root))).id();
        world.flush();
        assert!(world.get::<Collider>(late).is_none());
        world.entity_mut(root).remove::<NetHiddenUntilShown>();
        world.flush();
        for e in [root, child, late] {
            assert!(world.get::<Collider>(e).is_some(), "{e:?} got its collider back");
            assert!(world.get::<NetHiddenCollider>(e).is_none());
        }
    }

    /// A parent and a child hidden until different ticks: after revealing either one, only the
    /// colliders no hidden entity still covers are back. `restore_colliders` puts back the whole
    /// subtree, and `hide_new_collider` stashes the ones under a still-hidden proxy again in the
    /// same flush (audit framework5 #14: not a bug; this test guards that pairing).
    #[test]
    fn revealing_one_of_two_nested_hidden_proxies_keeps_the_other_hidden() {
        let hidden = |tick: u32| NetHiddenUntilShown {
            tick: NetTick(tick),
            previous: None,
        };
        for parent_first in [true, false] {
            let mut app = app();
            let world = app.world_mut();
            let parent = world.spawn((Collider::default(), hidden(5))).id();
            let child = world
                .spawn((Collider::default(), ChildOf(parent), hidden(9)))
                .id();
            let grandchild = world.spawn((Collider::default(), ChildOf(child))).id();
            world.flush();
            for e in [parent, child, grandchild] {
                assert!(world.get::<Collider>(e).is_none(), "{e:?} hidden");
            }
            let (first, second) = if parent_first {
                (parent, child)
            } else {
                (child, parent)
            };
            world.entity_mut(first).remove::<NetHiddenUntilShown>();
            world.flush();
            if parent_first {
                assert!(world.get::<Collider>(parent).is_some(), "the parent is shown");
            }
            for e in [child, grandchild] {
                assert!(
                    world.get::<Collider>(e).is_none(),
                    "parent_first {parent_first}: {e:?} is still under a hidden proxy"
                );
                assert!(world.get::<NetHiddenCollider>(e).is_some());
            }
            world.entity_mut(second).remove::<NetHiddenUntilShown>();
            world.flush();
            for e in [parent, child, grandchild] {
                assert!(world.get::<Collider>(e).is_some(), "{e:?} shown at last");
                assert!(world.get::<NetHiddenCollider>(e).is_none());
            }
        }
    }

    #[test]
    fn despawning_a_hidden_proxy_is_quiet() {
        let mut app = app();
        let world = app.world_mut();
        let root = world
            .spawn((
                Collider::default(),
                NetHiddenUntilShown {
                    tick: NetTick(1),
                    previous: None,
                },
            ))
            .id();
        world.flush();
        world.despawn(root);
        world.flush();
        assert!(world.get_entity(root).is_err());
    }

    /// Review finding rev-claude-29-k #5: a possession change made by a fixed-tick gameplay
    /// system must become `NetOwner` in the same tick, before `NetSet::Send` gathers replication
    /// (not one frame later in `PostUpdate`).
    #[test]
    fn possession_changes_apply_before_the_replication_send() {
        use std::time::Duration;

        use regia_app::prelude::TimeUpdateStrategy;
        use regia_net::{NetMode, NetSet};

        #[derive(Resource, Default)]
        struct Observed(Vec<bool>);

        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(regia_net::NetPlugin {
            mode: NetMode::DedicatedServer,
        });
        app.add_plugins(NetSessionBridgePlugin);
        app.finish_plugins();
        app.insert_resource(Observed::default());
        let (controller, pawn) = {
            let world = app.world_mut();
            world.insert_resource(PlayerManager::default());
            let player_entity = world
                .spawn(NetPlayer {
                    id: NetPlayerId {
                        peer: PeerId(7),
                        local: 0,
                    },
                })
                .id();
            let controller = PlayerManager::adopt_remote_static(world, player_entity)
                .expect("the remote player is adopted");
            (controller, world.spawn_empty().id())
        };
        app.add_systems(
            FixedUpdate,
            (move |world: &mut World| {
                if let Some(mut pc) = world.get_mut::<PlayerController>(controller) {
                    pc.possess(pawn);
                }
            })
            .in_set(NetSet::Gameplay),
        );
        app.add_systems(
            FixedUpdate,
            (move |world: &mut World| {
                let seen = world.get::<NetOwner>(pawn).is_some();
                world.resource_mut::<Observed>().0.push(seen);
            })
            .after(NetSet::Send),
        );
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(50)));
        app.update();
        let observed = &app.world().resource::<Observed>().0;
        assert!(!observed.is_empty(), "at least one fixed tick ran");
        assert!(
            observed[0],
            "the first fixed tick saw NetOwner before NetSet::Send (possession sync must run in FixedUpdate, before the replication send)"
        );
    }
}
