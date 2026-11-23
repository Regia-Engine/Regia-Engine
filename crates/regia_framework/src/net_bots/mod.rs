// SPDX-License-Identifier: MPL-2.0
//! Connectionless players driven by the server (spec 22 § T2.3, ADR-3346).
//!
//! [`NetBotsPlugin`] fills empty player slots, covers players during reconnect grace and hands
//! their pawn back on resume. A bot has a [`Player`], a controller and a pawn, but no transport
//! peer and no `InputCommand` queue. The `regia_ai::net_bots` adapter attaches the behaviour tree;
//! its leaf writes [`CharacterInput`] before `NetSet::Predict`.
//! Consequently movement, replication, lag compensation and game rules see the same pawn path as
//! they do for a remote participant.
//!
//! The framework owns policy and lifecycle only. `regia_ai` depends on this crate, so the tree,
//! perception and navigation implementation lives there rather than forming a dependency cycle.

use std::collections::{BTreeMap, BTreeSet};

use regia_app::prelude::*;
use regia_nav::NavAgentState;
use regia_net::replication::TransformReplication;
use regia_net::session::NetDespawnWithOwner;
use regia_net::session::net_session::NetDropped;
use regia_net::{NetMode, NetPlayer, NetPlayerId, Replicated};
use thiserror::Error;

use crate::character::CharacterInput;
use crate::net_character::NetCharacter;
use crate::net_session_bridge::NetRulesJoined;
use crate::player::{AiController, Player, PlayerController, PlayerManager};
use crate::rules::{GameRulesResource, RulesCtx, SpawnRequest};

/// Per-bot difficulty. All counters are fixed ticks, never wall-clock durations.
#[derive(Reflect, Debug, Clone, Copy, PartialEq)]
pub struct BotDifficulty {
    /// Ticks a new perception or patrol decision waits before it takes effect.
    pub reaction_ticks: u32,
    /// Largest absolute seeded yaw/pitch error, radians.
    pub aim_error_radians: f32,
    /// Sight radius, metres.
    pub perception_radius: f32,
}

impl Default for BotDifficulty {
    fn default() -> Self {
        Self {
            reaction_ticks: 12,
            aim_error_radians: 0.03,
            perception_radius: 40.0,
        }
    }
}

impl BotDifficulty {
    /// Finite, non-negative values suitable for a deterministic tick.
    #[must_use]
    pub fn sanitized(self) -> Self {
        Self {
            reaction_ticks: self.reaction_ticks,
            aim_error_radians: if self.aim_error_radians.is_finite() {
                self.aim_error_radians.clamp(0.0, std::f32::consts::PI)
            } else {
                Self::default().aim_error_radians
            },
            perception_radius: if self.perception_radius.is_finite() {
                self.perception_radius.max(0.0)
            } else {
                Self::default().perception_radius
            },
        }
    }
}

/// Server-bot policy. `max_bots = 0` disables all bot work.
#[derive(Resource, Debug, Clone, Copy, PartialEq)]
pub struct NetBotsConfig {
    /// Desired participants, including connected humans and grace slots.
    pub min_players: u32,
    /// Maximum filler plus takeover bots.
    pub max_bots: u32,
    /// Stable ticks before a surplus filler bot leaves after a human joins.
    pub join_delay_ticks: u32,
    /// Stable ticks before a missing filler bot is added after the initial fill.
    pub leave_delay_ticks: u32,
    /// Seed from which each bot's independent noise stream is derived.
    pub seed: u64,
    /// Difficulty assigned to newly added bots.
    pub difficulty: BotDifficulty,
}

impl From<regia_net::server_host::ServerBotSettings> for NetBotsConfig {
    fn from(settings: regia_net::server_host::ServerBotSettings) -> Self {
        Self {
            min_players: settings.min_players,
            max_bots: settings.max_bots,
            join_delay_ticks: settings.join_delay_ticks,
            leave_delay_ticks: settings.leave_delay_ticks,
            seed: settings.seed,
            difficulty: BotDifficulty {
                reaction_ticks: settings.reaction_ticks,
                aim_error_radians: settings.aim_error_radians,
                perception_radius: settings.perception_radius,
            },
        }
    }
}

impl Default for NetBotsConfig {
    fn default() -> Self {
        Self {
            min_players: 0,
            max_bots: 0,
            join_delay_ticks: 30,
            leave_delay_ticks: 30,
            seed: 0,
            difficulty: BotDifficulty::default(),
        }
    }
}

/// A bot participant. This is deliberately a marker, not a `PlayerKind` variant.
#[derive(Component, Reflect, Debug, Clone, Copy, Default, PartialEq)]
#[reflect(Component, Default)]
pub struct NetBot {
    /// Stable bot id within this server run.
    pub id: u32,
    /// Current difficulty.
    pub difficulty: BotDifficulty,
    /// The dropped player whose pawn this bot covers, if any.
    pub takeover_of: Option<NetPlayerId>,
}

/// On a pawn while a bot supplies its input. `net_character` gives this precedence over `NetOwner`.
#[derive(Component, Reflect, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[reflect(Component, Default)]
pub struct NetBotDriven {
    /// Owning bot id.
    pub bot: u32,
}

/// On a dropped player's character while a takeover bot drives it: the character's
/// `NetDespawnWithOwner` marker, lifted so the character stays with the bot when its player is gone
/// (KI-GP-033, ADR-10070), and put back when the bot hands the character back.
#[derive(Component, Debug, Clone, Copy, Default, PartialEq, Eq)]
struct NetBotHeldDespawnMark;

/// Deterministic state shared with the AI adapter. It is plain data so replay inspection can read it.
#[derive(Component, Reflect, Debug, Clone, Default, PartialEq)]
#[reflect(Component, Default)]
pub struct NetBotBrain {
    /// Owning bot id.
    pub bot: u32,
    /// Difficulty copied from the participant.
    pub difficulty: BotDifficulty,
    /// Independent seeded noise state.
    pub noise_state: u64,
    /// Ticks before the next decision takes effect.
    pub reaction_remaining: u32,
    /// Destination used by the current nav plan.
    pub planned_destination: Option<regia_core::math::Vec3>,
    /// Entity selected by perception, if any.
    pub perceived_target: Option<Entity>,
    /// Latest yaw with seeded difficulty noise, for a game's aiming code.
    pub aim_yaw: f32,
    /// Latest pitch with seeded difficulty noise, for a game's aiming code.
    pub aim_pitch: f32,
    /// Number of behaviour decisions made.
    pub decisions: u64,
}

impl NetBotBrain {
    /// A brain with an independent stream derived from the server seed and bot id.
    #[must_use]
    pub fn new(bot: u32, difficulty: BotDifficulty, seed: u64) -> Self {
        Self {
            bot,
            difficulty: difficulty.sanitized(),
            noise_state: seed ^ (u64::from(bot) << 32) ^ 0x9e37_79b9_7f4a_7c15,
            reaction_remaining: difficulty.reaction_ticks,
            ..Self::default()
        }
    }
}

/// One row in the combined session participant/scoreboard view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetSessionPlayerRow {
    /// `bot.N`, `peer.N` or `local.N`.
    pub player: String,
    /// Participant entity.
    pub entity: Entity,
    /// Possessed or rules-spawned pawn.
    pub pawn: Option<Entity>,
    /// Whether this row is a connectionless bot.
    pub is_bot: bool,
    /// Dropped player covered by this bot, written `peer.local`.
    pub takeover_of: Option<String>,
}

/// One bot row for `net.bots` and server UIs.
#[derive(Debug, Clone, PartialEq)]
pub struct NetBotRow {
    /// Bot id.
    pub id: u32,
    /// Always true, making bot identity explicit in generic player lists.
    pub is_bot: bool,
    /// Participant entity.
    pub player: Entity,
    /// Driven pawn.
    pub pawn: Option<Entity>,
    /// Reconnect player covered by this bot.
    pub takeover_of: Option<NetPlayerId>,
    /// Current difficulty.
    pub difficulty: BotDifficulty,
}

/// Errors from explicit bot administration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum NetBotError {
    /// `max_bots` would be exceeded.
    #[error("the server bot cap is reached")]
    AtCapacity,
    /// No such bot id.
    #[error("unknown bot")]
    Unknown,
    /// The process is not a listen or dedicated server.
    #[error("bots run only on a server")]
    NotServer,
    /// The player manager could not allocate a participant.
    #[error("no player slot is available")]
    PlayerSlot,
}

impl NetBotError {
    /// Stable agent error reason.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::AtCapacity => "bot_limit",
            Self::Unknown => "bot_unknown",
            Self::NotServer => "net_not_server",
            Self::PlayerSlot => "player_slot_unavailable",
        }
    }
}

/// Runtime policy state. Bot rows themselves live on ECS components for save/debug visibility.
#[derive(Resource, Debug, Default)]
pub struct NetBots {
    next_id: u32,
    active: BTreeMap<u32, Entity>,
    /// Bots added by administration (`add_bot`): the fill policy never removes them (ADR-5573).
    manual: BTreeSet<u32>,
    join_stable: u32,
    leave_stable: u32,
    started: bool,
}

impl NetBots {
    /// Number of active bots tracked by the policy.
    #[must_use]
    pub fn len(&self) -> usize {
        self.active.len()
    }

    /// Whether no bots are active.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.active.is_empty()
    }

    /// Participant entity of a bot.
    #[must_use]
    pub fn player_entity(&self, id: u32) -> Option<Entity> {
        self.active.get(&id).copied()
    }

    /// Bot ids in deterministic order.
    pub fn ids(&self) -> impl Iterator<Item = u32> + '_ {
        self.active.keys().copied()
    }
}

#[derive(Resource, Debug, Clone, Copy)]
struct NetBotsConfigSource {
    explicit: bool,
}

/// Initializes server-bot resources in a custom headless world.
///
/// [`NetBotsPlugin`] calls this and additionally registers reflection and schedules. Custom fixed-step
/// hosts can call it directly and drive [`net_bot_policy`] in their pre-step schedule.
pub fn install_net_bots(world: &mut World) {
    let explicit = world.contains_resource::<NetBotsConfig>();
    world.insert_resource(NetBotsConfigSource { explicit });
    if !explicit {
        let config = world
            .get_resource::<regia_net::server_host::ServerSettings>()
            .map_or_else(NetBotsConfig::default, |settings| settings.bots.into());
        world.insert_resource(config);
    }
    world.init_resource::<PlayerManager>();
    world.init_resource::<NetBots>();
}

/// Adds the server-bot policy. Add `regia_ai::net_bots::ServerBotAiPlugin` as well to drive pawns.
#[derive(Debug, Default, Clone, Copy)]
pub struct NetBotsPlugin;

impl Plugin for NetBotsPlugin {
    fn build(&self, app: &mut App) {
        install_net_bots(app.world_mut());
        app.register_type::<BotDifficulty>()
            .register_type::<NetBot>()
            .register_type::<NetBotDriven>()
            .register_type::<NetBotBrain>()
            .add_systems(
                PreUpdate,
                net_bot_policy.after(regia_net::session::net_session::net_session_system),
            );
    }
}

fn is_server(world: &World) -> bool {
    world
        .get_resource::<NetMode>()
        .is_none_or(|mode| matches!(*mode, NetMode::DedicatedServer | NetMode::ListenServer))
}

fn allocate_id(bots: &mut NetBots) -> Option<u32> {
    let id = bots.next_id.checked_add(1)?;
    bots.next_id = id;
    Some(id)
}

fn call_join_rules(world: &mut World, player: Entity) -> Option<SpawnRequest> {
    let mut rules = world.remove_resource::<GameRulesResource>().unwrap_or_default();
    let request = {
        let mut ctx = RulesCtx::new(world);
        rules.rules.on_player_joined(&mut ctx, player)
    };
    world.insert_resource(rules);
    request
}

/// Runs the game rules' leave callback for a player the [`PlayerManager`] could not remove (it
/// forgot the index, or is gone): the join callback ran for this player, so the leave must too,
/// before the player entity goes (w9-net-vehicle-steam-z 8).
fn call_leave_rules(world: &mut World, player: Entity) {
    if world.get_resource::<GameRulesResource>().is_some() {
        let mut rules = world.remove_resource::<GameRulesResource>().unwrap_or_default();
        let mut ctx = RulesCtx::new(world);
        rules.rules.on_player_left(&mut ctx, player);
        world.insert_resource(rules);
    }
}

fn spawn_participant(
    world: &mut World,
    id: u32,
    difficulty: BotDifficulty,
    seed: u64,
    existing_pawn: Option<Entity>,
    takeover_of: Option<NetPlayerId>,
) -> Result<Entity, NetBotError> {
    let Some((player, controller)) = PlayerManager::spawn_bot_static(world) else {
        return Err(NetBotError::PlayerSlot);
    };
    let request = call_join_rules(world, player);
    let pawn = match existing_pawn {
        Some(pawn) => {
            // A takeover keeps the dropped player's character after that player is gone.
            if let Ok(mut e) = world.get_entity_mut(pawn)
                && e.take::<NetDespawnWithOwner>().is_some()
            {
                e.insert(NetBotHeldDespawnMark);
            }
            Some(pawn)
        }
        None => request.map(|request| {
            let pawn = crate::player::spawn_pawn(world, &request);
            world.entity_mut(pawn).insert((
                Replicated {
                    transform: TransformReplication::Movement,
                    ..Replicated::default()
                },
                NetCharacter::default(),
            ));
            pawn
        }),
    };
    if let Some(pawn) = pawn {
        // The seat `MatchSessionRules` gave this bot, on the pawn it spawned or (a takeover) the
        // dropped player's pawn it keeps (ADR-8723).
        crate::rules::claim_pawn_seat(world, player, pawn);
        world
            .entity_mut(player)
            .insert(NetRulesJoined { pawn: Some(pawn) });
        if let Some(mut pc) = world.get_mut::<PlayerController>(controller) {
            pc.possess(pawn);
        }
        world.entity_mut(controller).insert(AiController::new(pawn));
        world.entity_mut(pawn).insert((
            NetBotDriven { bot: id },
            NetBotBrain::new(id, difficulty, seed),
            NavAgentState::default(),
        ));
    }
    world.entity_mut(player).insert(NetBot {
        id,
        difficulty: difficulty.sanitized(),
        takeover_of,
    });
    world.resource_mut::<NetBots>().active.insert(id, player);
    tracing::info!(target: "regia_framework", bot = id, pawn = pawn.map(Entity::to_bits), takeover = takeover_of.is_some(), "server bot joined");
    Ok(player)
}

/// Adds one bot immediately, bypassing join/leave hysteresis.
///
/// The bot stays until [`remove_bot`] (or `max_bots = 0`) removes it: the `min_players` fill
/// policy neither counts nor removes administratively added bots, though they use up `max_bots`
/// (ADR-5573).
pub fn add_bot(world: &mut World, difficulty: BotDifficulty) -> Result<u32, NetBotError> {
    let id = add_filler(world, difficulty)?;
    world.resource_mut::<NetBots>().manual.insert(id);
    Ok(id)
}

/// Adds one policy-owned filler bot.
fn add_filler(world: &mut World, difficulty: BotDifficulty) -> Result<u32, NetBotError> {
    if !is_server(world) {
        return Err(NetBotError::NotServer);
    }
    if world.resource::<NetBots>().active.len() >= world.resource::<NetBotsConfig>().max_bots as usize {
        return Err(NetBotError::AtCapacity);
    }
    let (id, seed) = {
        let mut bots = world.resource_mut::<NetBots>();
        let id = allocate_id(&mut bots).ok_or(NetBotError::PlayerSlot)?;
        (id, world.resource::<NetBotsConfig>().seed)
    };
    spawn_participant(world, id, difficulty, seed, None, None).map(|_| id)
}

fn remove_participant(world: &mut World, id: u32, despawn_pawn: bool) -> Result<(), NetBotError> {
    let Some(player) = world.resource_mut::<NetBots>().active.remove(&id) else {
        return Err(NetBotError::Unknown);
    };
    world.resource_mut::<NetBots>().manual.remove(&id);
    let pawn = world
        .get::<NetBot>(player)
        .and_then(|_| world.get::<NetRulesJoined>(player).and_then(|joined| joined.pawn));
    let index = world.get::<Player>(player).map(|p| p.index);
    if let Some(pawn) = pawn {
        if let Ok(mut entity) = world.get_entity_mut(pawn) {
            entity.remove::<NetBotDriven>();
            entity.remove::<NetBotBrain>();
            entity.remove::<NavAgentState>();
            // A takeover hands the character back with its leave rule (KI-GP-033).
            if entity.take::<NetBotHeldDespawnMark>().is_some() {
                entity.insert(NetDespawnWithOwner);
            }
        }
        if let Some(mut input) = world.get_mut::<CharacterInput>(pawn) {
            *input = CharacterInput::default();
        }
    }
    // A manager that does not know the index (a world restored or reloaded without it) removes
    // nothing: the entity goes anyway, never leaked (rc1-rev-net-k 10), and the rules' leave
    // callback still runs — with the player and pawn still readable, as on the manager's path.
    let removed = index.is_some_and(|index| PlayerManager::remove_player_static(world, index));
    if !removed && world.get_entity(player).is_ok() {
        call_leave_rules(world, player);
        // The player's controller entity goes with it, as `PlayerManager::remove_player` does.
        let controller = world.get::<Player>(player).map(|p| p.controller);
        if let Some(controller) = controller {
            world.despawn(controller);
        }
        world.despawn(player);
    }
    // Rules read score/health from the pawn during on_player_left; despawn only afterwards.
    if despawn_pawn
        && let Some(pawn) = pawn
        && world.get_entity(pawn).is_ok()
    {
        world.despawn(pawn);
    }
    tracing::info!(target: "regia_framework", bot = id, "server bot left");
    Ok(())
}

/// Removes a bot. A takeover bot hands its pawn back; a filler bot's pawn is despawned.
pub fn remove_bot(world: &mut World, id: u32) -> Result<(), NetBotError> {
    let takeover = world
        .resource::<NetBots>()
        .player_entity(id)
        .and_then(|entity| world.get::<NetBot>(entity))
        .is_some_and(|bot| bot.takeover_of.is_some());
    remove_participant(world, id, !takeover)
}

/// Changes one bot's reaction, aim and perception knobs.
pub fn set_bot_difficulty(world: &mut World, id: u32, difficulty: BotDifficulty) -> Result<(), NetBotError> {
    let player = world
        .resource::<NetBots>()
        .player_entity(id)
        .ok_or(NetBotError::Unknown)?;
    let difficulty = difficulty.sanitized();
    let pawn = world.get::<NetRulesJoined>(player).and_then(|joined| joined.pawn);
    if let Some(mut bot) = world.get_mut::<NetBot>(player) {
        bot.difficulty = difficulty;
    }
    if let Some(pawn) = pawn
        && let Some(mut brain) = world.get_mut::<NetBotBrain>(pawn)
    {
        brain.difficulty = difficulty;
        brain.reaction_remaining = difficulty.reaction_ticks;
    }
    Ok(())
}

/// Every bot in id order.
#[must_use]
pub fn net_bot_rows(world: &mut World) -> Vec<NetBotRow> {
    let mut query = world.query::<(Entity, &NetBot, Option<&NetRulesJoined>)>();
    let mut rows: Vec<_> = query
        .iter(world)
        .map(|(entity, bot, joined)| NetBotRow {
            id: bot.id,
            is_bot: true,
            player: entity,
            pawn: joined.and_then(|joined| joined.pawn),
            takeover_of: bot.takeover_of,
            difficulty: bot.difficulty,
        })
        .collect();
    rows.sort_by_key(|row| row.id);
    rows
}

/// Combined session players for lists and scoreboard rows, including the explicit `is_bot` flag.
#[must_use]
pub fn net_session_players(world: &mut World) -> Vec<NetSessionPlayerRow> {
    let mut query = world.query::<(
        Entity,
        Option<&Player>,
        Option<&NetPlayer>,
        Option<&NetBot>,
        Option<&NetRulesJoined>,
    )>();
    let mut rows = Vec::new();
    for (entity, player, remote, bot, joined) in query.iter(world) {
        if player.is_none() && remote.is_none() && bot.is_none() {
            continue;
        }
        let (name, pawn, is_bot, takeover_of) = if let Some(bot) = bot {
            (
                format!("bot.{}", bot.id),
                joined.and_then(|joined| joined.pawn),
                true,
                bot.takeover_of.map(|id| format!("{}.{}", id.peer.0, id.local)),
            )
        } else if let Some(remote) = remote {
            (
                format!("{}.{}", remote.id.peer.0, remote.id.local),
                joined.and_then(|joined| joined.pawn),
                false,
                None,
            )
        } else if let Some(player) = player {
            let pawn = world
                .get::<PlayerController>(player.controller)
                .map(|controller| controller.possessed)
                .filter(|pawn| *pawn != Entity::PLACEHOLDER);
            (format!("local.{}", player.index), pawn, false, None)
        } else {
            continue;
        };
        rows.push(NetSessionPlayerRow {
            player: name,
            entity,
            pawn,
            is_bot,
            takeover_of,
        });
    }
    rows.sort_by(|a, b| a.player.cmp(&b.player));
    rows
}

fn remote_entity(world: &mut World, id: NetPlayerId) -> Option<Entity> {
    let mut query = world.query::<(Entity, &NetPlayer)>();
    query
        .iter(world)
        .find_map(|(entity, player)| (player.id == id).then_some(entity))
}

fn prune_despawned_bots(world: &mut World) {
    let stale: Vec<u32> = world
        .resource::<NetBots>()
        .ids()
        .filter(|id| {
            world
                .resource::<NetBots>()
                .player_entity(*id)
                .is_none_or(|entity| world.get::<NetBot>(entity).is_none())
        })
        .collect();
    for id in stale {
        let mut bots = world.resource_mut::<NetBots>();
        bots.active.remove(&id);
        bots.manual.remove(&id);
    }
}

fn resume_takeovers(world: &mut World) {
    let takeovers: Vec<(u32, NetPlayerId)> = {
        let mut query = world.query::<&NetBot>();
        query
            .iter(world)
            .filter_map(|bot| bot.takeover_of.map(|player| (bot.id, player)))
            .collect()
    };
    let resumed: Vec<u32> = takeovers
        .into_iter()
        .filter_map(|(bot, id)| {
            let remote = remote_entity(world, id)?;
            (world.get::<NetDropped>(remote).is_none()).then_some(bot)
        })
        .collect();
    for id in resumed {
        let _ = remove_bot(world, id);
    }
}

fn finish_gone_takeovers(world: &mut World) {
    let takeovers: Vec<(u32, NetPlayerId)> = {
        let mut query = world.query::<&NetBot>();
        query
            .iter(world)
            .filter_map(|bot| bot.takeover_of.map(|player| (bot.id, player)))
            .collect()
    };
    let gone: Vec<u32> = takeovers
        .into_iter()
        .filter_map(|(bot, player)| remote_entity(world, player).is_none().then_some(bot))
        .collect();
    for id in gone {
        let entity = world.resource::<NetBots>().player_entity(id);
        if let Some(entity) = entity
            && let Some(mut bot) = world.get_mut::<NetBot>(entity)
        {
            bot.takeover_of = None;
        }
    }
}

fn cover_dropped(world: &mut World) {
    let dropped: Vec<(NetPlayerId, Entity)> = {
        let mut query = world.query_filtered::<(&NetPlayer, &NetRulesJoined), With<NetDropped>>();
        query
            .iter(world)
            .filter_map(|(player, joined)| joined.pawn.map(|pawn| (player.id, pawn)))
            .collect()
    };
    for (player, pawn) in dropped {
        if net_bot_rows(world)
            .iter()
            .any(|bot| bot.takeover_of == Some(player))
        {
            continue;
        }
        let at_cap =
            world.resource::<NetBots>().active.len() >= world.resource::<NetBotsConfig>().max_bots as usize;
        if at_cap {
            // The highest-id policy filler: never a takeover, never a bot an administrator added
            // (rc1-rev-net-k 3).
            let filler = policy_fillers(world).last().copied();
            if let Some(filler) = filler {
                let _ = remove_bot(world, filler);
            }
        }
        if world.resource::<NetBots>().active.len() >= world.resource::<NetBotsConfig>().max_bots as usize {
            continue;
        }
        let (id, seed, difficulty) = {
            let mut bots = world.resource_mut::<NetBots>();
            let Some(id) = allocate_id(&mut bots) else {
                continue;
            };
            let config = *world.resource::<NetBotsConfig>();
            (id, config.seed, config.difficulty)
        };
        let _ = spawn_participant(world, id, difficulty, seed, Some(pawn), Some(player));
    }
}

fn human_count(world: &mut World) -> u32 {
    let mut query = world.query::<&NetPlayer>();
    query.iter(world).count() as u32
}

/// Policy-owned fillers in id order: no takeover, not added by administration.
fn policy_fillers(world: &mut World) -> Vec<u32> {
    let rows = net_bot_rows(world);
    let manual = &world.resource::<NetBots>().manual;
    rows.into_iter()
        .filter(|bot| bot.takeover_of.is_none() && !manual.contains(&bot.id))
        .map(|bot| bot.id)
        .collect()
}

fn initial_fill(world: &mut World) {
    let config = *world.resource::<NetBotsConfig>();
    let wanted = config
        .min_players
        .saturating_sub(human_count(world))
        .min(config.max_bots);
    for _ in 0..wanted {
        if add_filler(world, config.difficulty).is_err() {
            break;
        }
    }
    world.resource_mut::<NetBots>().started = true;
}

fn reconcile_fillers(world: &mut World) {
    let config = *world.resource::<NetBotsConfig>();
    let fillers = policy_fillers(world);
    // Takeovers and administratively added bots use up `max_bots` but are not policy fillers.
    let others = world
        .resource::<NetBots>()
        .active
        .len()
        .saturating_sub(fillers.len());
    let wanted = config.min_players.saturating_sub(human_count(world)).min(
        config
            .max_bots
            .saturating_sub(u32::try_from(others).unwrap_or(u32::MAX)),
    ) as usize;
    let current = fillers.len();
    if current < wanted {
        world.resource_mut::<NetBots>().join_stable = 0;
        let stable = &mut world.resource_mut::<NetBots>().leave_stable;
        *stable = stable.saturating_add(1);
        let due = *stable >= config.leave_delay_ticks;
        if due {
            world.resource_mut::<NetBots>().leave_stable = 0;
            let _ = add_filler(world, config.difficulty);
        }
    } else if current > wanted {
        world.resource_mut::<NetBots>().leave_stable = 0;
        let stable = &mut world.resource_mut::<NetBots>().join_stable;
        *stable = stable.saturating_add(1);
        let due = *stable >= config.join_delay_ticks;
        if due {
            world.resource_mut::<NetBots>().join_stable = 0;
            if let Some(id) = fillers.last().copied() {
                let _ = remove_bot(world, id);
            }
        }
    } else {
        let mut bots = world.resource_mut::<NetBots>();
        bots.join_stable = 0;
        bots.leave_stable = 0;
    }
}

/// `PreUpdate`, after session events: handback, takeover, then hysteretic fill policy.
#[tracing::instrument(skip_all, target = "regia_framework", name = "net_bot_policy")]
pub fn net_bot_policy(world: &mut World) {
    let host_config = if world.resource::<NetBotsConfigSource>().explicit {
        None
    } else {
        world
            .get_resource::<regia_net::server_host::ServerSettings>()
            .map(|settings| settings.bots.into())
    };
    if let Some(config) = host_config {
        *world.resource_mut::<NetBotsConfig>() = config;
    }
    if !is_server(world) {
        return;
    }
    if world.resource::<NetBotsConfig>().max_bots == 0 {
        let ids: Vec<u32> = world.resource::<NetBots>().ids().collect();
        for id in ids {
            let _ = remove_bot(world, id);
        }
        world.resource_mut::<NetBots>().started = false;
        return;
    }
    prune_despawned_bots(world);
    resume_takeovers(world);
    finish_gone_takeovers(world);
    cover_dropped(world);
    if world.resource::<NetBots>().started {
        reconcile_fillers(world);
    } else {
        initial_fill(world);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::RulesPlugin;
    use regia_core::math::Vec3;
    use regia_scene::Transform;

    fn app(config: NetBotsConfig) -> App {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, RulesPlugin));
        app.insert_resource(config);
        app.add_plugins(NetBotsPlugin);
        app.world_mut().spawn((
            Transform::from_translation(Vec3::new(2.0, 0.0, 3.0)),
            crate::player::PlayerStart,
        ));
        app
    }

    #[test]
    fn no_clients_fill_min_players_immediately() {
        let mut app = app(NetBotsConfig {
            min_players: 4,
            max_bots: 4,
            ..NetBotsConfig::default()
        });
        app.update();
        let rows = net_bot_rows(app.world_mut());
        assert_eq!(rows.len(), 4);
        assert!(rows.iter().all(|row| row.is_bot && row.pawn.is_some()));
        assert_eq!(app.world().resource::<PlayerManager>().count(), 4);
        assert!(app.world().resource::<PlayerManager>().can_join());
        let players = net_session_players(app.world_mut());
        assert_eq!(players.len(), 4);
        assert!(players.iter().all(|row| row.is_bot));
    }

    #[test]
    fn joining_a_human_replaces_one_bot_only_after_hysteresis() {
        let mut app = app(NetBotsConfig {
            min_players: 4,
            max_bots: 4,
            join_delay_ticks: 2,
            ..NetBotsConfig::default()
        });
        app.update();
        app.world_mut().spawn(NetPlayer {
            id: NetPlayerId {
                peer: regia_net::PeerId(1),
                local: 0,
            },
        });
        app.update();
        assert_eq!(net_bot_rows(app.world_mut()).len(), 4);
        app.update();
        assert_eq!(net_bot_rows(app.world_mut()).len(), 3);
        app.update();
        assert_eq!(
            net_bot_rows(app.world_mut()).len(),
            3,
            "only one bot leaves per join"
        );
    }

    #[test]
    fn dropped_pawn_is_covered_and_the_same_pawn_is_handed_back() {
        let mut app = app(NetBotsConfig {
            min_players: 1,
            max_bots: 2,
            leave_delay_ticks: 99,
            ..NetBotsConfig::default()
        });
        let id = NetPlayerId {
            peer: regia_net::PeerId(8),
            local: 0,
        };
        let pawn = app.world_mut().spawn(CharacterInput::default()).id();
        let human = app
            .world_mut()
            .spawn((NetPlayer { id }, NetRulesJoined { pawn: Some(pawn) }))
            .id();
        app.world_mut().entity_mut(human).insert(NetDropped);
        app.update();
        let rows = net_bot_rows(app.world_mut());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].takeover_of, Some(id));
        assert_eq!(rows[0].pawn, Some(pawn));
        assert!(app.world().get::<NetBotDriven>(pawn).is_some());
        app.world_mut().entity_mut(human).remove::<NetDropped>();
        app.update();
        assert!(net_bot_rows(app.world_mut()).is_empty());
        assert!(app.world().get_entity(pawn).is_ok());
        assert!(app.world().get::<NetBotDriven>(pawn).is_none());
        assert!(app.world().get::<NetBotBrain>(pawn).is_none());
    }

    /// KI-GP-033 (ADR-10070): a takeover bot lifts the character's leave despawn while it drives
    /// (so the character stays with the bot if its player is gone) and hands it back with the
    /// character.
    #[test]
    fn a_takeover_lifts_the_leave_despawn_and_hands_it_back() {
        use regia_net::session::OwnedHandOff;
        use regia_net::session::players::default_hand_off;

        let mut app = app(NetBotsConfig {
            min_players: 1,
            max_bots: 2,
            leave_delay_ticks: 99,
            ..NetBotsConfig::default()
        });
        let id = NetPlayerId {
            peer: regia_net::PeerId(8),
            local: 0,
        };
        let pawn = app
            .world_mut()
            .spawn((
                CharacterInput::default(),
                regia_net::model::NetOwner(id.peer),
                NetDespawnWithOwner,
            ))
            .id();
        let human = app
            .world_mut()
            .spawn((NetPlayer { id }, NetRulesJoined { pawn: Some(pawn) }, NetDropped))
            .id();
        let gone = |_: regia_net::PeerId| false;
        assert_eq!(
            default_hand_off(app.world(), pawn, &gone),
            OwnedHandOff::Despawn,
            "before the takeover the character leaves with its player"
        );
        app.update();
        assert_eq!(net_bot_rows(app.world_mut())[0].takeover_of, Some(id));
        assert!(app.world().get::<NetDespawnWithOwner>(pawn).is_none());
        assert_eq!(
            default_hand_off(app.world(), pawn, &gone),
            OwnedHandOff::Server,
            "while a bot drives it, the character stays when its player is gone"
        );
        app.world_mut().entity_mut(human).remove::<NetDropped>();
        app.update();
        assert!(
            net_bot_rows(app.world_mut()).is_empty(),
            "the bot handed the character back"
        );
        assert!(
            app.world().get::<NetDespawnWithOwner>(pawn).is_some(),
            "with its leave rule"
        );
    }

    /// Spec 22 T2.3 addendum: when the dropped player's grace ends (its participant entity is
    /// gone), the takeover bot stays on as an ordinary filler (ADR-3346; test added by ADR-5628).
    #[test]
    fn a_takeover_becomes_a_filler_when_the_dropped_player_is_gone() {
        let mut app = app(NetBotsConfig {
            min_players: 1,
            max_bots: 2,
            leave_delay_ticks: 99,
            ..NetBotsConfig::default()
        });
        let id = NetPlayerId {
            peer: regia_net::PeerId(9),
            local: 0,
        };
        let pawn = app.world_mut().spawn(CharacterInput::default()).id();
        let human = app
            .world_mut()
            .spawn((NetPlayer { id }, NetRulesJoined { pawn: Some(pawn) }, NetDropped))
            .id();
        app.update();
        let rows = net_bot_rows(app.world_mut());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].takeover_of, Some(id));
        let bot = rows[0].id;

        app.world_mut().despawn(human);
        app.update();
        let rows = net_bot_rows(app.world_mut());
        assert_eq!(rows.len(), 1, "the bot stays: it fills the empty slot");
        assert_eq!(rows[0].id, bot, "the same bot");
        assert_eq!(rows[0].takeover_of, None, "no longer a takeover");
        // A later update keeps it: nothing is left to hand back to.
        app.update();
        assert_eq!(net_bot_rows(app.world_mut()).len(), 1);
    }

    #[test]
    fn bots_use_the_game_rules_join_and_leave_callbacks() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        struct Rules {
            joined: Arc<AtomicUsize>,
            left: Arc<AtomicUsize>,
            pawn_alive_on_left: Arc<AtomicBool>,
        }
        impl crate::rules::GameRules for Rules {
            fn on_player_joined(&mut self, _ctx: &mut RulesCtx, _player: Entity) -> Option<SpawnRequest> {
                self.joined.fetch_add(1, Ordering::Relaxed);
                Some(SpawnRequest::default())
            }

            fn on_player_left(&mut self, ctx: &mut RulesCtx, player: Entity) {
                self.left.fetch_add(1, Ordering::Relaxed);
                let alive = ctx
                    .world
                    .get::<NetRulesJoined>(player)
                    .and_then(|joined| joined.pawn)
                    .is_some_and(|pawn| ctx.world.get::<CharacterInput>(pawn).is_some());
                self.pawn_alive_on_left.store(alive, Ordering::Relaxed);
            }
        }

        let joined = Arc::new(AtomicUsize::new(0));
        let left = Arc::new(AtomicUsize::new(0));
        let pawn_alive_on_left = Arc::new(AtomicBool::new(false));
        let mut app = app(NetBotsConfig {
            max_bots: 1,
            ..NetBotsConfig::default()
        });
        app.insert_resource(GameRulesResource {
            rules: Box::new(Rules {
                joined: joined.clone(),
                left: left.clone(),
                pawn_alive_on_left: pawn_alive_on_left.clone(),
            }),
        });
        let id = add_bot(app.world_mut(), BotDifficulty::default()).expect("bot");
        assert_eq!(joined.load(Ordering::Relaxed), 1);
        remove_bot(app.world_mut(), id).expect("remove");
        assert_eq!(left.load(Ordering::Relaxed), 1);
        assert!(pawn_alive_on_left.load(Ordering::Relaxed));
    }

    #[test]
    fn setting_max_bots_to_zero_removes_existing_bots() {
        let mut app = app(NetBotsConfig {
            min_players: 2,
            max_bots: 2,
            ..NetBotsConfig::default()
        });
        app.update();
        assert_eq!(net_bot_rows(app.world_mut()).len(), 2);
        app.world_mut().resource_mut::<NetBotsConfig>().max_bots = 0;
        app.update();
        assert!(net_bot_rows(app.world_mut()).is_empty());
        assert_eq!(app.world().resource::<PlayerManager>().count(), 0);
    }

    #[test]
    fn administration_enforces_caps_and_updates_difficulty() {
        let mut app = app(NetBotsConfig {
            max_bots: 1,
            ..NetBotsConfig::default()
        });
        let id = add_bot(app.world_mut(), BotDifficulty::default()).expect("one bot");
        assert_eq!(
            add_bot(app.world_mut(), BotDifficulty::default()),
            Err(NetBotError::AtCapacity)
        );
        let difficulty = BotDifficulty {
            reaction_ticks: 2,
            aim_error_radians: 0.5,
            perception_radius: 12.0,
        };
        set_bot_difficulty(app.world_mut(), id, difficulty).expect("difficulty");
        assert_eq!(net_bot_rows(app.world_mut())[0].difficulty, difficulty);
        remove_bot(app.world_mut(), id).expect("remove");
        assert_eq!(remove_bot(app.world_mut(), id), Err(NetBotError::Unknown));
    }

    /// rc1-rev-net-k 3: covering a dropped player at `max_bots` may only evict a policy filler,
    /// never a bot an administrator added (`net.bots add`: "the fill policy never removes them").
    #[test]
    fn covering_a_dropped_player_never_evicts_an_added_bot() {
        let mut app = app(NetBotsConfig {
            min_players: 0,
            max_bots: 1,
            leave_delay_ticks: 99,
            ..NetBotsConfig::default()
        });
        app.update();
        let added = add_bot(app.world_mut(), BotDifficulty::default()).expect("bot");
        let id = NetPlayerId {
            peer: regia_net::PeerId(11),
            local: 0,
        };
        let pawn = app.world_mut().spawn(CharacterInput::default()).id();
        app.world_mut()
            .spawn((NetPlayer { id }, NetRulesJoined { pawn: Some(pawn) }, NetDropped));
        app.update();
        let rows = net_bot_rows(app.world_mut());
        assert_eq!(
            rows.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![added],
            "the added bot stays; at the cap with no filler, nobody covers"
        );
        assert_eq!(rows[0].takeover_of, None);
    }

    /// rc1-rev-net-k 10: a bot whose player the manager no longer knows (a world restored or
    /// reloaded without it) still leaves: its player entity is not leaked.
    #[test]
    fn a_bot_the_player_manager_forgot_still_leaves() {
        let mut app = app(NetBotsConfig {
            max_bots: 1,
            ..NetBotsConfig::default()
        });
        let id = add_bot(app.world_mut(), BotDifficulty::default()).expect("bot");
        let player = app
            .world()
            .resource::<NetBots>()
            .player_entity(id)
            .expect("player");
        app.world_mut().insert_resource(PlayerManager::default());
        remove_bot(app.world_mut(), id).expect("remove");
        assert!(
            app.world().get_entity(player).is_err(),
            "the player entity is gone"
        );
        assert!(net_bot_rows(app.world_mut()).is_empty());
    }

    /// w10-v051-smalls 2: the forgotten-bot path despawned the player but not its controller
    /// entity, which leaked one entity per removal. The entity count returns to its start.
    #[test]
    fn a_bot_the_player_manager_forgot_does_not_leak_its_controller() {
        let mut app = app(NetBotsConfig {
            max_bots: 1,
            ..NetBotsConfig::default()
        });
        let before = app.world().entities().len();
        let id = add_bot(app.world_mut(), BotDifficulty::default()).expect("bot");
        let player = app
            .world()
            .resource::<NetBots>()
            .player_entity(id)
            .expect("player");
        let controller = app
            .world()
            .get::<Player>(player)
            .expect("player component")
            .controller;
        assert!(
            app.world().get_entity(controller).is_ok(),
            "the controller exists"
        );
        app.world_mut().insert_resource(PlayerManager::default());
        remove_bot(app.world_mut(), id).expect("remove");
        assert!(
            app.world().get_entity(player).is_err(),
            "the player entity is gone"
        );
        assert!(
            app.world().get_entity(controller).is_err(),
            "the controller entity is gone too"
        );
        assert_eq!(app.world().entities().len(), before, "no entity leaked");
    }

    /// w9-net-vehicle-steam-z 8: the manager forgetting the index must not skip the game rules.
    /// The join callback ran for this bot, so `on_player_left` runs too — before the player
    /// entity goes, with the pawn still readable.
    #[test]
    fn a_bot_the_player_manager_forgot_still_notifies_the_rules() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct Rules {
            left: Arc<AtomicUsize>,
            saw_the_player: Arc<AtomicUsize>,
        }
        impl crate::rules::GameRules for Rules {
            fn on_player_joined(&mut self, _ctx: &mut RulesCtx, _player: Entity) -> Option<SpawnRequest> {
                Some(SpawnRequest::default())
            }

            fn on_player_left(&mut self, ctx: &mut RulesCtx, player: Entity) {
                self.left.fetch_add(1, Ordering::Relaxed);
                // What the callback may read: the player entity and its pawn, both still alive.
                let joined = ctx.world.get::<NetRulesJoined>(player);
                let pawn_alive = joined
                    .and_then(|joined| joined.pawn)
                    .is_some_and(|pawn| ctx.world.get::<CharacterInput>(pawn).is_some());
                if pawn_alive {
                    self.saw_the_player.fetch_add(1, Ordering::Relaxed);
                }
            }
        }

        let left = Arc::new(AtomicUsize::new(0));
        let saw_the_player = Arc::new(AtomicUsize::new(0));
        let mut app = app(NetBotsConfig {
            max_bots: 1,
            ..NetBotsConfig::default()
        });
        app.insert_resource(GameRulesResource {
            rules: Box::new(Rules {
                left: Arc::clone(&left),
                saw_the_player: Arc::clone(&saw_the_player),
            }),
        });
        let id = add_bot(app.world_mut(), BotDifficulty::default()).expect("bot");
        assert_eq!(left.load(Ordering::Relaxed), 0);
        // The manager loses track of the index (a restored or reloaded world).
        app.world_mut().insert_resource(PlayerManager::default());
        remove_bot(app.world_mut(), id).expect("remove");
        assert_eq!(
            left.load(Ordering::Relaxed),
            1,
            "on_player_left runs even when the manager forgot the index"
        );
        assert_eq!(
            saw_the_player.load(Ordering::Relaxed),
            1,
            "the rules could still read the player and its pawn"
        );
    }

    #[test]
    fn an_added_bot_survives_the_fill_policy() {
        // `net.bots add` returns success; the fill policy must not quietly remove that bot later.
        let mut app = app(NetBotsConfig {
            min_players: 0,
            max_bots: 2,
            join_delay_ticks: 2,
            ..NetBotsConfig::default()
        });
        app.update();
        let id = add_bot(app.world_mut(), BotDifficulty::default()).expect("bot");
        for _ in 0..6 {
            app.update();
        }
        let rows = net_bot_rows(app.world_mut());
        assert_eq!(rows.iter().map(|row| row.id).collect::<Vec<_>>(), vec![id]);
    }

    #[test]
    fn an_added_bot_uses_the_cap_but_not_the_fill() {
        // min 2, max 2: two policy fillers. An admin bot cannot exceed max_bots, and once a human
        // joins the policy removes its own filler, never the admin bot.
        let mut app = app(NetBotsConfig {
            min_players: 2,
            max_bots: 3,
            join_delay_ticks: 1,
            ..NetBotsConfig::default()
        });
        app.update();
        assert_eq!(net_bot_rows(app.world_mut()).len(), 2);
        let admin = add_bot(app.world_mut(), BotDifficulty::default()).expect("third bot");
        assert_eq!(
            add_bot(app.world_mut(), BotDifficulty::default()),
            Err(NetBotError::AtCapacity)
        );
        app.world_mut().spawn(NetPlayer {
            id: NetPlayerId {
                peer: regia_net::PeerId(1),
                local: 0,
            },
        });
        for _ in 0..4 {
            app.update();
        }
        let ids: Vec<u32> = net_bot_rows(app.world_mut()).iter().map(|row| row.id).collect();
        assert_eq!(ids.len(), 2, "one policy filler leaves for the human: {ids:?}");
        assert!(ids.contains(&admin), "the admin bot stays: {ids:?}");
        remove_bot(app.world_mut(), admin).expect("remove");
        app.update();
        assert_eq!(net_bot_rows(app.world_mut()).len(), 1);
    }

    #[test]
    fn difficulty_sanitizes_non_finite_knobs() {
        let d = BotDifficulty {
            reaction_ticks: 0,
            aim_error_radians: f32::NAN,
            perception_radius: f32::INFINITY,
        }
        .sanitized();
        assert!(d.aim_error_radians.is_finite());
        assert!(d.perception_radius.is_finite());
        assert_eq!(d.perception_radius, BotDifficulty::default().perception_radius);
    }
}
