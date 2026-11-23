// SPDX-License-Identifier: MPL-2.0
//! The match core as ECS glue for a server-authoritative game (`book/agents/match-rules.md`,
//! "Using the core from ECS").
//!
//! [`MatchSessionPlugin`] steps the [`MatchRules`] session once per fixed tick in its own
//! [`MatchRulesSet`]: it builds the [`Contestant`] list from entities with [`TeamId`],
//! [`Transform`] and [`Health`], writes each objective's position onto the entity tagged
//! [`ObjectiveEntity`], and sends each [`RulesEvent`] as a [`MatchEvent`] message.
//! [`MatchSessionRules`] is a [`GameRules`] implementation over the same session: it seats joining
//! players, sends `Leave` for leaving ones, records deaths, ends the match with the board, and
//! builds the scoreboard from the team scores and the per-seat ledger.
//!
//! The adapter is glue only: every rule (captures, hill points, phases, kills, assists) is the
//! `regia_rules` core's arithmetic, unchanged. A lockstep game keeps stepping its own session and
//! does not add this plugin.
//!
//! # Contestant order
//!
//! The core resolves simultaneous touches in the order of the contestant list (the first
//! toucher picks up a flag). The adapter sorts contestants by the arrival order of [`TeamId`]
//! (`regia_ecs::spawn_order::SpawnOrder<TeamId>`, tracked from the moment the plugin is added),
//! then by entity index and bits. It never uses the order archetypes are stored in, so inserting
//! the same components in another order (another archetype) gives the same board. A contestant's
//! id is its entity index.

use std::collections::BTreeMap;

use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_ecs::spawn_order::{SpawnKey, SpawnOrder, track_spawn_order};
use regia_rules::match_flow::{FlowInput, SeatId};
use regia_rules::objective::{Contestant, RulesEvent, ScoreReason};
use regia_rules::phases::MatchPhase;
use regia_rules::session::MatchSession;
use regia_rules::teams::SeatRequest;
use regia_scene::Transform;

use super::{
    GameRules, GameRulesRegistry, LedgerSlot, MatchRules, MatchRulesPlugin, RulesCtx, ScoreSheet,
    SpawnRequest, TeamId, default_spawn_point, moved_seat, retag_moved_seats, score_sheet,
};
use crate::damage::{DamageSet, Health};

/// The `FixedUpdate` set the session steps in. Opt-in: only [`MatchSessionPlugin`] configures it,
/// so apps without the plugin keep their fixed-step schedule (and determinism pins) as they were.
/// Order it after the game's damage and death handling so a kill lands on the step it happened.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MatchRulesSet;

/// One event of the match core ([`RulesEvent`]), sent as an ECS message in the order the session
/// returned it from its step.
#[derive(Message, Debug, Clone, PartialEq)]
pub struct MatchEvent(pub RulesEvent);

/// The live unit entity of each [`MatchEvent`] the adapter sent that names a unit, keyed by the
/// message id (the `id` of the `MessageId` that `World::write_message` returns). The core names a unit by its
/// contestant id (the entity index), and Bevy reuses an index once the entity is gone, so a
/// reader that resolves the index again later can reach another entity. This records the entity
/// (index and generation) while it is still that contestant, on the step the event happened
/// (rules-logic-nodes-review finding 2). Entries are dropped once the message buffer no longer
/// holds the message.
#[derive(Resource, Debug, Clone, Default)]
pub struct MatchEventUnits {
    units: BTreeMap<usize, Entity>,
}

impl MatchEventUnits {
    /// The unit entity of message `id` when the adapter sent it, as it was on that step. `None` for
    /// an event that names no unit, one whose unit was not a contestant of that step, or one the
    /// adapter did not send (a client's replicated event, a message a test wrote). Check the
    /// entity is still alive before using it.
    pub fn unit(&self, id: usize) -> Option<Entity> {
        self.units.get(&id).copied()
    }

    /// Recorded entries (bounded by the messages the buffer still holds).
    pub fn len(&self) -> usize {
        self.units.len()
    }

    /// True when nothing is recorded.
    pub fn is_empty(&self) -> bool {
        self.units.is_empty()
    }
}

/// Tags the entity that shows objective `id` (a flag or the hill: `Objective::entity_id` on the
/// board). After each step the adapter writes the objective's `position` onto its [`Transform`].
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[reflect(Component)]
pub struct ObjectiveEntity(pub u32);

/// Inserted on an entity whose latest death the damage step already credited to the session's
/// ledger (killer and victim both seated). [`MatchSessionRules::on_character_died`] removes it
/// instead of recording that death a second time.
///
/// Reflected as an opaque value (registered by [`MatchRulesPlugin`] and [`MatchSessionPlugin`]), so
/// a world snapshot keeps the "already credited" bit between the damage step and the death
/// callback; the snapshot prints it by name (`DeathCredited`), not as an empty struct.
#[derive(
    Component, Reflect, Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize,
)]
#[reflect(opaque)]
#[reflect(Component, Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeathCredited;

/// The adapter's state: present when [`MatchSessionPlugin`] is installed (the agent protocol's
/// `rules.get` answers `match_session: true` then).
///
/// Reflected as one opaque value (serde and `Debug`), like [`MatchRules`], so
/// [`crate::state_hash::insert_resource_text`] with `"MatchSessionAdapter"` adds it to a world
/// snapshot and a restore keeps which player holds which seat.
#[derive(Resource, Reflect, Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[reflect(opaque)]
#[reflect(Resource, Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MatchSessionAdapter {
    /// Fixed steps the adapter ran.
    pub steps: u64,
    /// Contestants of the last step.
    pub contestants: usize,
    /// Events the last step sent.
    pub last_events: usize,
    /// Seats [`MatchSessionRules`] gave to joined players, keyed by the player entity. Kept in
    /// step with the session's seat moves (a team change or a launch rebalance) by the adapter's
    /// fixed step. [`claim_pawn_seat`] tags a player's pawn with its entry.
    pub seats: BTreeMap<Entity, SeatId>,
}

/// Steps the [`MatchRules`] session from ECS once per fixed tick (see the module documentation).
///
/// Adds [`MatchRulesPlugin`] when missing, the [`MatchEvent`] message, the [`MatchSessionAdapter`]
/// resource, spawn-order tracking of [`TeamId`] and the `match_session_step` system in
/// [`MatchRulesSet`]. Orders [`MatchRulesSet`] after [`DamageSet`], so a death lands on the step it
/// happened whatever order the game added its plugins in (a schedule that forces the set before
/// [`DamageSet`] is a cycle, which bevy refuses). Registers [`MatchSessionRules`] by name in the
/// rules registry, creating the registry when missing, so `RulesPlugin` may come before or after.
/// Install [`MatchSessionRules`] in `GameRulesResource` (or name it in a level) to route joins,
/// leaves, deaths and the scoreboard through the session too.
pub struct MatchSessionPlugin;

impl Plugin for MatchSessionPlugin {
    fn build(&self, app: &mut App) {
        if !app.has_plugin(std::any::type_name::<MatchRulesPlugin>()) {
            app.add_plugins(MatchRulesPlugin);
        }
        // Before any entity holds `TeamId`: the hooks can only be installed on first use.
        track_spawn_order::<TeamId>(app.world_mut());
        app.add_message::<MatchEvent>()
            .init_resource::<MatchSessionAdapter>()
            .init_resource::<MatchEventUnits>()
            .register_type::<ObjectiveEntity>()
            .register_type::<super::MatchEventListener>()
            .register_type::<DeathCredited>()
            .register_type::<MatchSessionAdapter>()
            .register_type::<SpawnOrder<TeamId>>()
            // The edge also exists without `DamagePlugin`: configuring the set creates it.
            .configure_sets(FixedUpdate, MatchRulesSet.after(DamageSet))
            .add_systems(FixedUpdate, step_match_session.in_set(MatchRulesSet));
        // `RulesPlugin` keeps an existing registry (`init_resource`), so the name stays registered
        // whichever of the two plugins the game adds first.
        app.init_resource::<GameRulesRegistry>();
        app.world_mut()
            .resource_mut::<GameRulesRegistry>()
            .register_default::<MatchSessionRules>();
    }
}

/// The sort key of a contestant: [`SpawnOrder`] of [`TeamId`] when tracked, else the entity id.
fn contestant_key(order: Option<&SpawnOrder<TeamId>>, entity: Entity) -> SpawnKey {
    match order {
        Some(order) => order.key(entity),
        None => (u64::MAX, entity.index().index(), entity.to_bits()),
    }
}

/// Whether a contestant with `health` is alive: its current health is above zero. A NaN
/// `current` (set through reflection or a script; [`Health::new`] never makes one) is not alive,
/// although `Health::is_dead` says it is not dead either (`NaN <= 0` is false), so it can never
/// touch a flag or hold a hill.
fn is_alive(health: &Health) -> bool {
    health.current > 0.0
}

/// The session's contestants, in the adapter's stable order, with each unit's seat when it has a
/// [`LedgerSlot`] on a fighting team.
pub fn match_contestants(world: &mut World) -> Vec<(Contestant, Option<SeatId>)> {
    contestant_entities(world)
        .into_iter()
        .map(|(_, c, seat)| (c, seat))
        .collect()
}

/// [`match_contestants`] with each contestant's entity.
fn contestant_entities(world: &mut World) -> Vec<(Entity, Contestant, Option<SeatId>)> {
    let mut query = world.query::<(Entity, &TeamId, &Transform, &Health, Option<&LedgerSlot>)>();
    let world: &World = world;
    let order = world.get_resource::<SpawnOrder<TeamId>>();
    let mut units: Vec<(SpawnKey, Entity, Contestant, Option<SeatId>)> = query
        .iter(world)
        .map(|(entity, team, transform, health, slot)| {
            let t = transform.translation;
            let contestant = Contestant {
                id: entity.index().index(),
                team: team.0,
                position: [t.x, t.y, t.z],
                alive: is_alive(health),
            };
            let seat = slot
                .filter(|_| team.0 != 0)
                .map(|slot| SeatId::new(team.0, slot.0));
            (contestant_key(order, entity), entity, contestant, seat)
        })
        .collect();
    units.sort_by_key(|(key, ..)| *key);
    units.into_iter().map(|(_, e, c, seat)| (e, c, seat)).collect()
}

/// `FixedUpdate` ([`MatchRulesSet`]): one session step over the ECS contestants, then the
/// objective mirror and the [`MatchEvent`]s.
fn step_match_session(world: &mut World) {
    if !world.contains_resource::<MatchRules>() {
        return;
    }
    let _span = tracing::trace_span!(target: "regia_framework", "match_session_step").entered();
    // Seat moves since the last step (`rules.match_assign`, a host's `assign_team`): tags first,
    // so this step binds and credits the seats the players hold now. Then the seats a leave
    // outside `on_player_left` emptied.
    follow_seat_moves(world);
    forget_unheld_seats(world);
    let units = contestant_entities(world);
    let report = {
        let mut rules = world.resource_mut::<MatchRules>();
        let session = &mut rules.session;
        // Bind each seated unit to its row so captures and hill points credit the right seat. A
        // pawn whose player left keeps its tags; its seat is not held, so it binds nothing.
        for (_, unit, seat) in &units {
            let Some(seat) = seat else { continue };
            if !session.flow.seated.contains(seat) {
                continue;
            }
            let bound = session.ledgers.row(*seat).map(|row| row.unit);
            if matches!(bound, Some(current) if current != Some(unit.id)) {
                let _ = session.ledgers.bind_unit(*seat, unit.id);
            }
        }
        let contestants: Vec<Contestant> = units.iter().map(|(_, c, _)| *c).collect();
        session.step(&contestants)
    };
    // A launch in this step may have rebalanced the teams: retag before the next damage step.
    follow_seat_moves(world);
    mirror_objectives(world);
    if let Some(mut adapter) = world.get_resource_mut::<MatchSessionAdapter>() {
        adapter.steps = adapter.steps.saturating_add(1);
        adapter.contestants = units.len();
        adapter.last_events = report.events.len();
    }
    // Contestant ids are entity indices, unique among live entities, so one step maps each id to
    // one entity.
    let by_id: BTreeMap<u32, Entity> = units.iter().map(|(e, c, _)| (c.id, *e)).collect();
    let mut recorded = Vec::new();
    for event in report.events {
        let message = MatchEvent(event);
        let unit = message.fields().unit.and_then(|id| by_id.get(&id).copied());
        if let (Some(id), Some(unit)) = (world.write_message(message), unit) {
            recorded.push((id.id, unit));
        }
    }
    let oldest = world
        .get_resource::<Messages<MatchEvent>>()
        .map(Messages::oldest_message_count);
    if let Some(mut store) = world.get_resource_mut::<MatchEventUnits>() {
        if let Some(oldest) = oldest {
            // Messages the buffer dropped can no longer be read: forget their units.
            store.units = store.units.split_off(&oldest);
        }
        store.units.extend(recorded);
    }
}

/// Drains the session's seat moves onto the ECS side: the [`TeamId`] and [`LedgerSlot`] tags
/// ([`retag_moved_seats`]) and the player seats of [`MatchSessionAdapter`], so a later leave sends
/// `Leave` for the seat the player holds now. Nothing happens without moves.
fn follow_seat_moves(world: &mut World) {
    let moves = match world.get_resource::<MatchRules>() {
        Some(rules) if !rules.session.seat_moves.is_empty() => rules.session.seat_moves.clone(),
        _ => return,
    };
    if let Some(mut adapter) = world.get_resource_mut::<MatchSessionAdapter>() {
        for seat in adapter.seats.values_mut() {
            if let Some(to) = moved_seat(&moves, *seat) {
                *seat = to;
            }
        }
    }
    retag_moved_seats(world);
}

/// Removes [`LedgerSlot`] from every entity that wears `seat` ([`TeamId`] and [`LedgerSlot`]).
/// [`TeamId`] stays: the entity is still on its team (friendly fire, perception), it just credits
/// no ledger row. Entities are visited in the world's order; removal does not depend on it.
fn release_seat_tags(world: &mut World, seat: SeatId) {
    let mut query = world.query::<(Entity, &TeamId, &LedgerSlot)>();
    let wearing: Vec<Entity> = query
        .iter(world)
        .filter(|(_, team, slot)| team.0 == seat.team && slot.0 == seat.slot)
        .map(|(entity, ..)| entity)
        .collect();
    for entity in wearing {
        world.entity_mut(entity).remove::<LedgerSlot>();
    }
}

/// Forgets the players whose seat the session no longer holds (a `Leave` sent through
/// `rules.match_input` or a host's own `apply`, not [`MatchSessionRules::on_player_left`]) and
/// releases that seat's tags, as `on_player_left` does.
fn forget_unheld_seats(world: &mut World) {
    let Some(rules) = world.get_resource::<MatchRules>() else {
        return;
    };
    let seated = &rules.session.flow.seated;
    let Some(adapter) = world.get_resource::<MatchSessionAdapter>() else {
        return;
    };
    let gone: Vec<(Entity, SeatId)> = adapter
        .seats
        .iter()
        .filter(|(_, seat)| !seated.contains(seat))
        .map(|(player, seat)| (*player, *seat))
        .collect();
    for (player, seat) in gone {
        world.resource_mut::<MatchSessionAdapter>().seats.remove(&player);
        release_seat_tags(world, seat);
    }
}

/// Tags `pawn` with the seat [`MatchSessionRules`] gave `player`: inserts [`TeamId`] and
/// [`LedgerSlot`] and returns the seat. Returns `None` and changes nothing when the player holds
/// no seat (another rules type, a refused seat, a player who left).
///
/// The framework's join flows call it right after they spawn the pawn a join asked for, and a
/// bot takeover calls it on the pawn it keeps. It is keyed by the player, so a join that spawns
/// nothing never tags somebody else's pawn. A game with its own join flow calls it the same way.
pub fn claim_pawn_seat(world: &mut World, player: Entity, pawn: Entity) -> Option<SeatId> {
    let seat = world
        .get_resource::<MatchSessionAdapter>()?
        .seats
        .get(&player)
        .copied()?;
    let held = world
        .get_resource::<MatchRules>()
        .is_some_and(|rules| rules.session.flow.seated.contains(&seat));
    if !held {
        return None;
    }
    world
        .get_entity_mut(pawn)
        .ok()?
        .insert((TeamId(seat.team), LedgerSlot(seat.slot)));
    Some(seat)
}

/// Writes each objective's `position` onto the [`Transform`] of the entity tagged with its id.
/// Untouched when the position did not change, so change detection only fires on a move.
fn mirror_objectives(world: &mut World) {
    let positions: BTreeMap<u32, [f32; 3]> = world
        .resource::<MatchRules>()
        .session
        .board
        .objectives
        .iter()
        .map(|objective| (objective.entity_id, objective.position))
        .collect();
    let mut query = world.query::<(&ObjectiveEntity, &mut Transform)>();
    for (tag, mut transform) in query.iter_mut(world) {
        let Some(&[x, y, z]) = positions.get(&tag.0) else {
            continue;
        };
        let target = Vec3::new(x, y, z);
        if transform.translation != target {
            transform.translation = target;
        }
    }
}

/// Seats `player` in the hosted session and remembers the seat in [`MatchSessionAdapter`].
///
/// `team` asks for a team (`None` or `0` assigns automatically: fewest seats, then lowest team
/// score, then lowest id). A player already seated keeps its seat. Errors are the core's
/// ([`regia_rules::RulesError`]); nothing changes then.
pub fn seat_player(
    world: &mut World,
    player: Entity,
    team: Option<u8>,
) -> Result<SeatId, regia_rules::RulesError> {
    if let Some(seat) = world
        .get_resource::<MatchSessionAdapter>()
        .and_then(|adapter| adapter.seats.get(&player).copied())
    {
        return Ok(seat);
    }
    world.init_resource::<MatchRules>();
    let request = SeatRequest {
        team: team.filter(|team| *team != 0),
        ..SeatRequest::default()
    };
    let seats = world
        .resource_mut::<MatchRules>()
        .session
        .assign_team(&request, &[])?;
    let seat = seats
        .first()
        .copied()
        .ok_or(regia_rules::RulesError::SeatMissing)?;
    world.init_resource::<MatchSessionAdapter>();
    world
        .resource_mut::<MatchSessionAdapter>()
        .seats
        .insert(player, seat);
    Ok(seat)
}

/// Records one death on `seat`'s ledger row without a killer (a fall, a kill volume, a hit from an
/// unseated source): deaths go up by one, the score does not change, the victim's pending assist
/// hits are dropped, and a `player_scored` event (`death`) is queued for the next step.
///
/// A seat nobody holds is ignored: the pawn of a player who left keeps its [`TeamId`] and
/// [`LedgerSlot`], and recording its death would open a row the next occupant would inherit.
pub fn record_death(session: &mut MatchSession, seat: SeatId) {
    if !session.flow.seated.contains(&seat) {
        return;
    }
    let tick = session.flow.match_tick;
    let index = session.ledgers.ensure(seat);
    let Some(row) = session.ledgers.rows.get_mut(index) else {
        return;
    };
    row.deaths = row.deaths.saturating_add(1);
    let event = RulesEvent::PlayerScored {
        tick,
        team: row.team,
        slot: row.slot,
        reason: ScoreReason::Death,
        score: row.score,
    };
    session.ledgers.contributions.retain(|list| list.seat != seat);
    session.board.events.push(event);
}

/// The standings of a hosted session: [`score_sheet`] (one row per seat from the ledger), with
/// each team heading followed by the team's board score (`"Team 2: 3"`, or `"<name>: 3"` for a
/// named team). A localization key (`loc:` prefix) keeps its heading unchanged.
#[must_use]
pub fn match_session_score_sheet(session: &MatchSession) -> ScoreSheet {
    let mut sheet = score_sheet(session);
    for team in &mut sheet.teams {
        if team.name.starts_with("loc:") {
            continue;
        }
        let score = session
            .board
            .team_scores
            .get(usize::from(team.id))
            .copied()
            .unwrap_or(0);
        let label = if team.name.is_empty() {
            format!("Team {}", team.id)
        } else {
            team.name.clone()
        };
        team.name = format!("{label}: {score}");
    }
    sheet
}

/// A [`GameRules`] implementation over the [`MatchRules`] session (see the module documentation).
///
/// - `on_player_joined` seats the player ([`seat_player`]; a [`TeamId`] on the player entity asks
///   for that team) and returns a spawn request at the default spawn point with
///   `team: Some(rules team id)`. The join flow tags the pawn it spawns (or keeps) with
///   [`claim_pawn_seat`]: [`TeamId`] and [`LedgerSlot`].
///   A refused seat (every team full) returns `None`: the player watches.
/// - `on_player_left` sends `FlowInput::Leave` for the player's seat and removes [`LedgerSlot`]
///   from the entities that wore it (the pawn keeps [`TeamId`]).
/// - `on_character_died` records the death on the character's seat, unless the damage step
///   already credited it ([`DeathCredited`]).
/// - `is_match_over` reads the board (or the post-match phase).
/// - `scoreboard` is [`match_session_score_sheet`].
///
/// State lives in the [`MatchRules`] and [`MatchSessionAdapter`] resources, so the rules object
/// itself is empty and survives hot reload.
#[derive(Debug, Default, Clone, Copy)]
pub struct MatchSessionRules;

impl GameRules for MatchSessionRules {
    fn on_player_joined(&mut self, ctx: &mut RulesCtx, player: Entity) -> Option<SpawnRequest> {
        let preferred = ctx.world.get::<TeamId>(player).map(|team| team.0);
        let seat = match seat_player(ctx.world, player, preferred) {
            Ok(seat) => seat,
            Err(error) => {
                tracing::warn!(
                    target: "regia_framework",
                    code = error.code(),
                    "match session refused a seat for player {}: {error}; the player watches",
                    player.to_bits()
                );
                return None;
            }
        };
        let translation = default_spawn_point(ctx.world, player);
        Some(SpawnRequest {
            transform: Transform {
                translation,
                rotation: Quat::IDENTITY,
                scale: Vec3::ONE,
            },
            prefab: None,
            team: Some(seat.team),
        })
    }

    fn on_player_left(&mut self, ctx: &mut RulesCtx, player: Entity) {
        let Some(seat) = ctx
            .world
            .get_resource_mut::<MatchSessionAdapter>()
            .and_then(|mut adapter| adapter.seats.remove(&player))
        else {
            return;
        };
        // The pawn may stay in the world (a corpse, a bot takeover that keeps it). Without its
        // seat tag its hits and kills credit no row, so the seat's next occupant starts clean.
        release_seat_tags(ctx.world, seat);
        let Some(mut rules) = ctx.world.get_resource_mut::<MatchRules>() else {
            return;
        };
        let leave = FlowInput::Leave {
            team: seat.team,
            slot: seat.slot,
        };
        if let Err(error) = rules.session.apply(&leave) {
            tracing::warn!(
                target: "regia_framework",
                code = error.code(),
                "match session refused the leave of seat {}/{}: {error}",
                seat.team,
                seat.slot
            );
        }
    }

    fn on_character_died(&mut self, ctx: &mut RulesCtx, character: Entity) {
        let Ok(mut entity) = ctx.world.get_entity_mut(character) else {
            return;
        };
        if entity.take::<DeathCredited>().is_some() {
            return;
        }
        let (Some(team), Some(slot)) = (
            entity.get::<TeamId>().copied(),
            entity.get::<LedgerSlot>().copied(),
        ) else {
            return;
        };
        if team.0 == 0 {
            return;
        }
        if let Some(mut rules) = ctx.world.get_resource_mut::<MatchRules>() {
            record_death(&mut rules.session, SeatId::new(team.0, slot.0));
        }
    }

    fn is_match_over(&self, world: &World) -> bool {
        world.get_resource::<MatchRules>().is_some_and(|rules| {
            rules.session.board.match_over || rules.session.flow.phases.phase == MatchPhase::PostMatch
        })
    }

    fn scoreboard(&mut self, ctx: &mut RulesCtx) -> Option<ScoreSheet> {
        let rules = ctx.world.get_resource::<MatchRules>()?;
        Some(match_session_score_sheet(&rules.session))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_rules::mode::MatchMode;
    use regia_rules::objective::ObjectiveSettings;
    use regia_rules::session::SessionSettings;

    const RED_BASE: [f32; 3] = [-20.0, 0.0, 0.0];
    const BLUE_BASE: [f32; 3] = [20.0, 0.0, 0.0];
    const RED_FLAG: u32 = 900;
    const BLUE_FLAG: u32 = 901;

    /// Every [`MatchEvent`] the adapter sent, in order.
    #[derive(Resource, Default)]
    struct EventLog(Vec<RulesEvent>);

    fn log_events(mut reader: MessageReader<MatchEvent>, mut log: ResMut<EventLog>) {
        log.0.extend(reader.read().map(|event| event.0.clone()));
    }

    /// A headless app with the adapter and a capture-the-flag session (capture limit `limit`)
    /// whose two flags sit at their bases.
    fn ctf_app(limit: u32) -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        let mut session = MatchSession::new(SessionSettings {
            objectives: ObjectiveSettings {
                capture_limit: limit,
                ..ObjectiveSettings::default()
            },
            ..SessionSettings::default()
        });
        session.board.reset(MatchMode::CaptureTheFlag);
        session.board.spawn_flag(RED_FLAG, 1, RED_BASE).expect("red flag");
        session
            .board
            .spawn_flag(BLUE_FLAG, 2, BLUE_BASE)
            .expect("blue flag");
        app.insert_resource(MatchRules { session });
        app.add_plugins(MatchSessionPlugin);
        app.init_resource::<EventLog>();
        app.add_systems(FixedUpdate, log_events.after(MatchRulesSet));
        app
    }

    fn step(app: &mut App) {
        app.world_mut().run_schedule(FixedUpdate);
    }

    fn at(position: [f32; 3]) -> Transform {
        Transform::from_translation(Vec3::from(position))
    }

    fn move_to(app: &mut App, entity: Entity, position: [f32; 3]) {
        if let Some(mut transform) = app.world_mut().get_mut::<Transform>(entity) {
            transform.translation = Vec3::from(position);
        }
    }

    /// Joins `player` through [`MatchSessionRules`] and spawns its pawn the way the framework's
    /// join flows do (`crate::player::spawn_pawn`, then [`claim_pawn_seat`]), with health.
    fn join(app: &mut App, player: Entity) -> (SeatId, Entity) {
        let world = app.world_mut();
        let request = MatchSessionRules
            .on_player_joined(&mut RulesCtx::new(world), player)
            .expect("seated players spawn");
        let pawn = crate::player::spawn_pawn(world, &request);
        let claimed = claim_pawn_seat(world, player, pawn);
        world.entity_mut(pawn).insert(Health::new(100.0));
        let seat = world.resource::<MatchSessionAdapter>().seats[&player];
        assert_eq!(claimed, Some(seat), "the join flow tags the pawn it spawned");
        assert_eq!(request.team, Some(seat.team), "the request names the rules team");
        (seat, pawn)
    }

    fn board_debug(app: &App) -> String {
        format!("{:?}", app.world().resource::<MatchRules>().session.board)
    }

    /// rules-review-fixes-b (logic-nodes review 2): the adapter records the live entity of the unit
    /// an event names, by message id, and forgets it once the message buffer drops the message.
    #[test]
    fn match_event_units_record_the_named_unit_and_forget_dropped_messages() {
        let mut app = ctf_app(5);
        let red_player = app.world_mut().spawn_empty().id();
        let (_, red) = join(&mut app, red_player);
        move_to(&mut app, red, BLUE_BASE);
        step(&mut app);
        let taken: Vec<usize> = {
            let world = app.world();
            let messages = world.resource::<Messages<MatchEvent>>();
            let mut cursor = messages.get_cursor();
            cursor
                .read_with_id(messages)
                .filter(|(event, _)| matches!(event.0, RulesEvent::FlagTaken { .. }))
                .map(|(_, id)| id.id)
                .collect()
        };
        assert_eq!(taken.len(), 1, "one flag_taken: {}", board_debug(&app));
        let units = app.world().resource::<MatchEventUnits>();
        assert_eq!(
            units.unit(taken[0]),
            Some(red),
            "the taker's entity, index and generation"
        );
        assert_eq!(units.len(), 1, "only events that name a unit are recorded");
        // Two buffer swaps drop the message; the next step forgets its unit.
        for _ in 0..2 {
            app.world_mut().resource_mut::<Messages<MatchEvent>>().update();
        }
        step(&mut app);
        let units = app.world().resource::<MatchEventUnits>();
        assert_eq!(units.unit(taken[0]), None);
        assert!(units.is_empty(), "nothing the buffer still holds names a unit");
    }

    #[test]
    fn match_session_rules_seat_players_and_end_the_match_on_the_capture_limit() {
        let mut app = ctf_app(1);
        let red_player = app.world_mut().spawn_empty().id();
        let blue_player = app.world_mut().spawn_empty().id();
        let (red_seat, red) = join(&mut app, red_player);
        let (blue_seat, blue) = join(&mut app, blue_player);
        // Automatic assignment: the first joiner takes team 1, the second the emptier team 2.
        assert_eq!(red_seat, SeatId::new(1, 0));
        assert_eq!(blue_seat, SeatId::new(2, 0));
        assert_eq!(app.world().get::<TeamId>(red), Some(&TeamId(1)));
        assert_eq!(app.world().get::<LedgerSlot>(blue), Some(&LedgerSlot(0)));
        assert_eq!(
            app.world().resource::<MatchRules>().session.flow.seated,
            vec![red_seat, blue_seat]
        );
        let red_flag = app
            .world_mut()
            .spawn((at(RED_BASE), ObjectiveEntity(RED_FLAG)))
            .id();
        let blue_flag = app
            .world_mut()
            .spawn((at(BLUE_BASE), ObjectiveEntity(BLUE_FLAG)))
            .id();
        move_to(&mut app, red, RED_BASE);
        move_to(&mut app, blue, [0.0, 0.0, 30.0]);
        step(&mut app);
        assert!(!MatchSessionRules.is_match_over(app.world()));

        // The raid: red runs to the blue base, takes the flag, carries it home and scores.
        move_to(&mut app, red, BLUE_BASE);
        step(&mut app);
        move_to(&mut app, red, [0.0, 0.0, 0.0]);
        step(&mut app);
        assert_eq!(
            app.world().get::<Transform>(blue_flag).map(|t| t.translation),
            Some(Vec3::ZERO),
            "the carried flag's entity follows its carrier"
        );
        move_to(&mut app, red, RED_BASE);
        step(&mut app);

        assert!(
            MatchSessionRules.is_match_over(app.world()),
            "one capture reaches the limit"
        );
        let red_id = red.index().index();
        let log = &app.world().resource::<EventLog>().0;
        assert!(log.iter().any(|event| matches!(
            event,
            RulesEvent::FlagTaken { unit, flag: BLUE_FLAG, .. } if *unit == red_id
        )));
        assert!(log.iter().any(|event| matches!(
            event,
            RulesEvent::FlagCaptured { unit, team: 1, .. } if *unit == red_id
        )));
        assert!(
            log.iter()
                .any(|event| matches!(event, RulesEvent::MatchOver { team: 1, .. }))
        );
        let session = &app.world().resource::<MatchRules>().session;
        assert_eq!(session.board.team_scores.get(1), Some(&1));
        let row = session.ledgers.row(red_seat).expect("red row");
        assert_eq!(row.unit, Some(red_id), "the adapter bound the pawn to its seat");
        assert_eq!(row.objective_points, 1, "the carrier's ledger has the capture");
        assert_eq!(
            app.world().get::<Transform>(blue_flag).map(|t| t.translation),
            Some(Vec3::from(BLUE_BASE)),
            "a captured flag goes home"
        );
        assert_eq!(
            app.world().get::<Transform>(red_flag).map(|t| t.translation),
            Some(Vec3::from(RED_BASE))
        );

        // A death without a seated killer is recorded once; a credited one is not recorded again.
        let world = app.world_mut();
        MatchSessionRules.on_character_died(&mut RulesCtx::new(world), blue);
        world.entity_mut(red).insert(DeathCredited);
        MatchSessionRules.on_character_died(&mut RulesCtx::new(world), red);
        assert!(
            world.get::<DeathCredited>(red).is_none(),
            "the marker is consumed"
        );
        let session = &world.resource::<MatchRules>().session;
        assert_eq!(session.ledgers.row(blue_seat).map(|row| row.deaths), Some(1));
        assert_eq!(session.ledgers.row(red_seat).map(|row| row.deaths), Some(0));

        // Leaving sends `Leave`: the seat and its ledger row go.
        MatchSessionRules.on_player_left(&mut RulesCtx::new(world), blue_player);
        let session = &world.resource::<MatchRules>().session;
        assert_eq!(session.flow.seated, vec![red_seat]);
        assert!(session.ledgers.row(blue_seat).is_none());
        assert!(
            !world
                .resource::<MatchSessionAdapter>()
                .seats
                .contains_key(&blue_player)
        );
        // The departed player's pawn keeps its team but not its seat; its death opens no row for
        // the next occupant of that seat.
        assert_eq!(world.get::<TeamId>(blue), Some(&TeamId(2)));
        assert!(world.get::<LedgerSlot>(blue).is_none());
        MatchSessionRules.on_character_died(&mut RulesCtx::new(world), blue);
        assert!(
            world
                .resource::<MatchRules>()
                .session
                .ledgers
                .row(blue_seat)
                .is_none()
        );
    }

    #[derive(Component)]
    struct MarkerA;

    #[derive(Component)]
    struct MarkerB;

    /// Two team-2 raiders that reach the red flag on the same step, spawned in the same order on
    /// both apps. `pre_create_b` first makes the archetype of the second raider (MarkerB), so its
    /// archetype is stored before the first raider's.
    fn raid_app(pre_create_b: bool) -> (App, Entity, Entity) {
        let mut app = ctf_app(3);
        // Same entity index and TeamId arrival on both apps; no health, so never a contestant.
        let pre = if pre_create_b {
            app.world_mut()
                .spawn((MarkerB, TeamId(2), at([0.0, 0.0, 90.0]), Health::new(1.0)))
                .id()
        } else {
            app.world_mut()
                .spawn((MarkerA, TeamId(2), at([0.0, 0.0, 90.0]), Health::new(1.0)))
                .id()
        };
        app.world_mut().entity_mut(pre).remove::<Health>();
        let first = app
            .world_mut()
            .spawn((TeamId(2), at([0.0, 0.0, 40.0]), Health::new(100.0)))
            .id();
        let second = app
            .world_mut()
            .spawn((Health::new(100.0), at([0.0, 0.0, 50.0]), MarkerB, TeamId(2)))
            .id();
        (app, first, second)
    }

    fn archetype_order(app: &mut App) -> Vec<Entity> {
        let mut query = app
            .world_mut()
            .query_filtered::<Entity, (With<TeamId>, With<Health>)>();
        query.iter(app.world()).collect()
    }

    #[test]
    fn contestants_follow_spawn_order_not_archetype_order() {
        let (mut plain, a_first, a_second) = raid_app(false);
        let (mut swapped, b_first, b_second) = raid_app(true);
        assert_eq!(archetype_order(&mut plain), vec![a_first, a_second]);
        assert_eq!(
            archetype_order(&mut swapped),
            vec![b_second, b_first],
            "the second app stores the raiders' archetypes the other way round"
        );
        for (app, first, second) in [(&mut plain, a_first, a_second), (&mut swapped, b_first, b_second)] {
            step(app);
            move_to(app, first, RED_BASE);
            move_to(app, second, RED_BASE);
            step(app);
            let carrier = app
                .world()
                .resource::<MatchRules>()
                .session
                .board
                .objective(RED_FLAG)
                .and_then(|flag| flag.carrier_id);
            assert_eq!(
                carrier,
                Some(first.index().index()),
                "the first raider in spawn order takes the flag"
            );
            let order: Vec<u32> = match_contestants(app.world_mut())
                .iter()
                .map(|(unit, _)| unit.id)
                .collect();
            assert_eq!(order, vec![first.index().index(), second.index().index()]);
        }
        assert_eq!(a_first.index(), b_first.index());
        assert_eq!(board_debug(&plain), board_debug(&swapped), "identical board bits");
        let session = |app: &App| app.world().resource::<MatchRules>().session.clone();
        assert_eq!(session(&plain), session(&swapped));
    }

    #[test]
    fn objective_positions_are_mirrored_onto_their_entities() {
        let mut app = ctf_app(3);
        let red_flag = app
            .world_mut()
            .spawn((at([1.0, 2.0, 3.0]), ObjectiveEntity(RED_FLAG)))
            .id();
        let stray = app
            .world_mut()
            .spawn((at([7.0, 7.0, 7.0]), ObjectiveEntity(4242)))
            .id();
        let untagged = app.world_mut().spawn(at([8.0, 8.0, 8.0])).id();
        step(&mut app);
        assert_eq!(
            app.world().get::<Transform>(red_flag).map(|t| t.translation),
            Some(Vec3::from(RED_BASE)),
            "the flag entity moves to the flag's position on the first step"
        );
        let raider = app
            .world_mut()
            .spawn((TeamId(2), at(RED_BASE), Health::new(100.0)))
            .id();
        step(&mut app);
        for position in [[4.0, 0.0, -6.0], [-3.5, 1.25, 9.0]] {
            move_to(&mut app, raider, position);
            step(&mut app);
            assert_eq!(
                app.world().get::<Transform>(red_flag).map(|t| t.translation),
                Some(Vec3::from(position)),
                "the carried flag follows its carrier"
            );
        }
        // The carrier dies: the flag drops where it fell and stays there.
        if let Some(mut health) = app.world_mut().get_mut::<Health>(raider) {
            health.current = 0.0;
        }
        step(&mut app);
        move_to(&mut app, raider, [0.0, 0.0, 0.0]);
        step(&mut app);
        assert_eq!(
            app.world().get::<Transform>(red_flag).map(|t| t.translation),
            Some(Vec3::new(-3.5, 1.25, 9.0))
        );
        assert_eq!(
            app.world().get::<Transform>(stray).map(|t| t.translation),
            Some(Vec3::splat(7.0)),
            "a tag with no objective is left alone"
        );
        assert_eq!(
            app.world().get::<Transform>(untagged).map(|t| t.translation),
            Some(Vec3::splat(8.0))
        );
        assert_eq!(app.world().resource::<MatchSessionAdapter>().steps, 6);
    }

    #[test]
    fn the_adapter_is_opt_in_and_registers_its_rules() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, crate::rules::RulesPlugin));
        assert!(!app.world().contains_resource::<MatchSessionAdapter>());
        app.add_plugins(MatchSessionPlugin);
        assert!(app.world().contains_resource::<MatchRules>());
        assert!(
            app.world()
                .resource::<GameRulesRegistry>()
                .registered_names()
                .contains(&"MatchSessionRules")
        );
        assert!(app.world().resource::<SpawnOrder<TeamId>>().is_tracking());
    }

    #[test]
    fn the_scoreboard_heads_each_team_with_its_board_score() {
        let mut session = MatchSession::default();
        session.teams.defs[1].name_key = "Blue".into();
        session.teams.defs[2].name_key = "loc:team.green".into();
        session.board.team_scores = vec![0, 4, 2, 0, 0];
        session
            .assign_team(
                &SeatRequest {
                    team: Some(1),
                    ..SeatRequest::default()
                },
                &[],
            )
            .expect("seat");
        let sheet = match_session_score_sheet(&session);
        let names: Vec<&str> = sheet.teams.iter().map(|team| team.name.as_str()).collect();
        assert_eq!(names, ["Team 1: 4", "Blue: 2", "loc:team.green", "Team 4: 0"]);
        assert_eq!(sheet.rows.len(), 1);
        assert_eq!(sheet.rows[0].team, Some(1));
    }

    /// Review proof. A leave and a rejoin of the same coordinate must not reuse the scoreboard
    /// row id: clients break sort ties on that id, so a reused id is the departed player's row.
    #[test]
    fn a_leave_and_rejoin_gets_a_new_scoreboard_row_id() {
        let mut session = MatchSession::default();
        session
            .apply(&FlowInput::Seat { team: 1, slot: 0 })
            .expect("seat");
        let first = match_session_score_sheet(&session).rows[0].id;
        session
            .apply(&FlowInput::Leave { team: 1, slot: 0 })
            .expect("leave");
        session
            .apply(&FlowInput::Seat { team: 1, slot: 0 })
            .expect("rejoin");
        let second = match_session_score_sheet(&session).rows[0].id;
        assert_ne!(
            first, second,
            "scoreboard id {first:#x} was reused for the next occupant of seat 1/0"
        );
    }

    /// The damage step orders itself before [`crate::script::ScriptsSet`], so that set has to
    /// exist before the fixed schedule runs.
    fn add_damage(app: &mut App) {
        app.configure_sets(FixedUpdate, crate::script::ScriptsSet::FixedUpdate);
        app.add_plugins(crate::damage::DamagePlugin);
    }

    /// Review proof. `record_death` ignores a seat nobody holds. A kill from that pawn, which
    /// still wears `TeamId` and `LedgerSlot`, must not open the row again either.
    #[test]
    fn a_departed_pawns_kill_does_not_reopen_its_row() {
        let mut app = ctf_app(3);
        add_damage(&mut app);
        let red_player = app.world_mut().spawn_empty().id();
        let blue_player = app.world_mut().spawn_empty().id();
        let (_red_seat, red) = join(&mut app, red_player);
        let (blue_seat, blue) = join(&mut app, blue_player);
        let world = app.world_mut();
        MatchSessionRules.on_player_left(&mut RulesCtx::new(world), blue_player);
        assert!(
            world
                .resource::<MatchRules>()
                .session
                .ledgers
                .row(blue_seat)
                .is_none()
        );
        world.write_message(crate::damage::DamageEvent {
            target: red,
            amount: 1_000.0,
            source: Some(blue),
            position: None,
        });
        step(&mut app);
        assert!(
            app.world()
                .resource::<MatchRules>()
                .session
                .ledgers
                .row(blue_seat)
                .is_none(),
            "the departed pawn's kill reopened seat {}/{}",
            blue_seat.team,
            blue_seat.slot
        );
    }

    /// Review proof. The seat of a join is delivered only to the pawn that join spawns. A later
    /// `spawn_pawn` (a bot takeover keeps the dropped player's pawn and does not spawn) must not
    /// wear it.
    #[test]
    fn a_join_without_a_spawn_does_not_stamp_the_next_pawn() {
        let mut app = ctf_app(3);
        let watcher = app.world_mut().spawn_empty().id();
        let _request = MatchSessionRules
            .on_player_joined(&mut RulesCtx::new(app.world_mut()), watcher)
            .expect("one seat is free");
        let seated = app
            .world()
            .resource::<MatchSessionAdapter>()
            .seats
            .get(&watcher)
            .copied()
            .expect("the join recorded a seat");
        let pawn = crate::player::spawn_pawn(
            app.world_mut(),
            &SpawnRequest {
                transform: at([1.0, 0.0, 1.0]),
                prefab: None,
                team: None,
            },
        );
        assert!(
            app.world().get::<TeamId>(pawn).is_none(),
            "spawn_pawn stamped seat {}/{} from a join that did not spawn this pawn",
            seated.team,
            seated.slot
        );
        assert!(app.world().get::<LedgerSlot>(pawn).is_none());
    }

    /// Review proof. The fixed step reads `MatchRules`, `MatchSessionAdapter` and, on the death
    /// path, `DeathCredited`. A world snapshot has to carry them or a restore-then-step is not
    /// the uninterrupted step. Contestant order also reads `SpawnOrder<TeamId>`.
    #[test]
    fn the_fixed_step_state_is_in_the_world_snapshot() {
        let mut app = ctf_app(3);
        let player = app.world_mut().spawn_empty().id();
        let _ = join(&mut app, player);
        step(&mut app);
        app.world_mut()
            .spawn((at(RED_BASE), ObjectiveEntity(RED_FLAG), DeathCredited));
        let world = app.world();
        let mut snapshot = crate::state_hash::world_snapshot(world);
        let mut missing = Vec::new();
        if !crate::state_hash::insert_resource_text(world, &mut snapshot, "MatchRules") {
            missing.push("MatchRules");
        }
        if !crate::state_hash::insert_resource_text(world, &mut snapshot, "MatchSessionAdapter") {
            missing.push("MatchSessionAdapter");
        }
        let reflected_order = world
            .get_resource::<regia_ecs::reflect::AppTypeRegistry>()
            .is_some_and(|registry| {
                registry
                    .read()
                    .get(std::any::TypeId::of::<SpawnOrder<TeamId>>())
                    .is_some()
            });
        if !reflected_order {
            missing.push("SpawnOrder<TeamId>");
        }
        let text = snapshot.values().cloned().collect::<Vec<_>>().join("\n");
        if !text.contains("DeathCredited") {
            missing.push("DeathCredited");
        }
        assert!(
            missing.is_empty(),
            "fixed-step state missing from the snapshot: {missing:?}"
        );
    }

    fn flag_carrier(app: &App) -> Option<u32> {
        app.world()
            .resource::<MatchRules>()
            .session
            .board
            .objective(RED_FLAG)
            .and_then(|flag| flag.carrier_id)
    }

    /// How the game orders the damage step and the session step in [`raid_killed_this_tick`].
    #[derive(Clone, Copy, Debug)]
    enum GameOrder {
        /// The game chains `DamageSet` before `MatchRulesSet` (what the arena does).
        DamageFirst,
        /// The game chains `MatchRulesSet` before `DamageSet`.
        StepFirst,
        /// The game adds no order; only the plugins do.
        PluginsOnly,
    }

    /// One step where a living raider stands on the enemy flag and a `DamageEvent` kills them,
    /// under the game's `order`. `Err` when bevy refuses to build that schedule (a cycle with
    /// the plugin's own order), else the red flag's carrier after the step.
    fn raid_killed_this_tick(order: GameOrder) -> Result<Option<u32>, String> {
        let mut app = ctf_app(3);
        add_damage(&mut app);
        match order {
            GameOrder::DamageFirst => {
                app.configure_sets(FixedUpdate, (crate::damage::DamageSet, MatchRulesSet).chain());
            }
            GameOrder::StepFirst => {
                app.configure_sets(FixedUpdate, (MatchRulesSet, crate::damage::DamageSet).chain());
            }
            GameOrder::PluginsOnly => {}
        }
        let built = app
            .world_mut()
            .try_schedule_scope(FixedUpdate, |world, schedule| {
                schedule.initialize(world).map(|_| ())
            });
        if let Ok(Err(error)) = built {
            return Err(format!("{error:?}"));
        }
        let raider = app
            .world_mut()
            .spawn((TeamId(2), at(RED_BASE), Health::new(1.0)))
            .id();
        app.world_mut().write_message(crate::damage::DamageEvent {
            target: raider,
            amount: 10.0,
            source: None,
            position: None,
        });
        step(&mut app);
        Ok(flag_carrier(&app))
    }

    /// Review proof. Whether the raider takes the flag on the tick they die must not depend on
    /// which of `DamageSet` and `MatchRulesSet` the schedule runs first. The plugin orders the
    /// session step after damage (ADR-8723), so a game that adds no order gets the damage-first
    /// result, and a game that forces the step first gets a schedule bevy refuses (a cycle)
    /// instead of a different board. Before the fix the step-first schedule ran and its raider
    /// took the flag.
    #[test]
    fn a_death_on_the_same_tick_does_not_depend_on_schedule_order() {
        let damage_first = raid_killed_this_tick(GameOrder::DamageFirst).expect("damage first runs");
        assert_eq!(damage_first, None, "the dead raider takes nothing");
        let plugins_only = raid_killed_this_tick(GameOrder::PluginsOnly).expect("no game order runs");
        assert_eq!(
            damage_first, plugins_only,
            "damage-before-step carrier {damage_first:?}, plugin-order carrier {plugins_only:?}"
        );
        if let Ok(step_first) = raid_killed_this_tick(GameOrder::StepFirst) {
            assert_eq!(
                damage_first, step_first,
                "damage-before-step carrier {damage_first:?}, step-before-damage carrier {step_first:?}"
            );
        }
    }

    /// Review proof. A non-finite `Health` is not a living contestant (`NaN <= 0` is false, so
    /// `Health::is_dead` says it is alive).
    #[test]
    fn a_non_finite_health_does_not_take_a_flag() {
        let mut app = ctf_app(3);
        app.world_mut().spawn((
            TeamId(2),
            at(RED_BASE),
            Health {
                current: f32::NAN,
                max: 100.0,
            },
        ));
        step(&mut app);
        assert!(
            flag_carrier(&app).is_none(),
            "a pawn with NaN health took the flag"
        );
    }

    /// The rules name is registered whichever of `RulesPlugin` and `MatchSessionPlugin` comes
    /// first (review nit 1).
    #[test]
    fn the_rules_name_is_registered_before_the_rules_plugin_too() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(MatchSessionPlugin);
        app.add_plugins(crate::rules::RulesPlugin);
        let names = app.world().resource::<GameRulesRegistry>().registered_names();
        assert!(names.contains(&"MatchSessionRules"), "{names:?}");
        assert!(names.contains(&"DefaultRules"), "{names:?}");
    }

    /// Only a finite or infinite positive health is alive; NaN, zero and negatives are not
    /// (finding G).
    #[test]
    fn only_positive_health_is_a_living_contestant() {
        let health = |current: f32| Health { current, max: 100.0 };
        assert!(is_alive(&health(1.0)));
        assert!(is_alive(&health(f32::INFINITY)), "an invulnerable pawn plays");
        for dead in [0.0, -0.0, -1.0, f32::NEG_INFINITY, f32::NAN] {
            assert!(!is_alive(&health(dead)), "{dead} is not alive");
        }
    }

    /// Each join's seat goes to the pawn of that join only: two joins before any spawn keep
    /// their own seats, and a takeover tags the pawn it keeps with the new player's seat
    /// (finding C, ADR-8723).
    #[test]
    fn claims_are_keyed_by_the_player() {
        let mut app = ctf_app(3);
        let first = app.world_mut().spawn_empty().id();
        let second = app.world_mut().spawn_empty().id();
        let world = app.world_mut();
        let first_request = MatchSessionRules
            .on_player_joined(&mut RulesCtx::new(world), first)
            .expect("first seat");
        let _ = MatchSessionRules
            .on_player_joined(&mut RulesCtx::new(world), second)
            .expect("second seat");
        let first_seat = world.resource::<MatchSessionAdapter>().seats[&first];
        let second_seat = world.resource::<MatchSessionAdapter>().seats[&second];
        assert_ne!(first_seat, second_seat);
        // The second player's pawn is spawned first; it wears the second seat.
        let second_pawn = crate::player::spawn_pawn(world, &first_request);
        assert_eq!(claim_pawn_seat(world, second, second_pawn), Some(second_seat));
        // A takeover keeps a pawn that wore some other seat: the claim retags it.
        let kept = world.spawn((TeamId(4), LedgerSlot(7))).id();
        assert_eq!(claim_pawn_seat(world, first, kept), Some(first_seat));
        assert_eq!(world.get::<TeamId>(kept), Some(&TeamId(first_seat.team)));
        assert_eq!(world.get::<LedgerSlot>(kept), Some(&LedgerSlot(first_seat.slot)));
        assert_eq!(world.get::<TeamId>(second_pawn), Some(&TeamId(second_seat.team)));
        // A player without a seat claims nothing.
        let watcher = world.spawn_empty().id();
        let bare = world.spawn_empty().id();
        assert_eq!(claim_pawn_seat(world, watcher, bare), None);
        assert!(world.get::<TeamId>(bare).is_none());
    }

    /// A `Leave` sent to the session directly (the agent's `rules.match_input`), not through
    /// `on_player_left`, releases the seat tag on the next step too (finding B, adapter half).
    #[test]
    fn a_leave_outside_the_rules_hook_releases_the_seat_on_the_next_step() {
        let mut app = ctf_app(3);
        let player = app.world_mut().spawn_empty().id();
        let (seat, pawn) = join(&mut app, player);
        app.world_mut()
            .resource_mut::<MatchRules>()
            .session
            .apply(&FlowInput::Leave {
                team: seat.team,
                slot: seat.slot,
            })
            .expect("leave");
        step(&mut app);
        assert!(app.world().get::<LedgerSlot>(pawn).is_none());
        assert_eq!(app.world().get::<TeamId>(pawn), Some(&TeamId(seat.team)));
        assert!(
            !app.world()
                .resource::<MatchSessionAdapter>()
                .seats
                .contains_key(&player)
        );
        let session = &app.world().resource::<MatchRules>().session;
        assert!(session.ledgers.row(seat).is_none(), "nothing reopened the row");
    }

    /// A team change between matches moves the pawn's tags and the player's seat in the next
    /// fixed step, so a later leave empties the seat the player holds (ADR-8657's seat moves,
    /// drained by the adapter).
    #[test]
    fn a_team_change_follows_the_player_and_the_pawn() {
        use regia_rules::teams::TeamSwitch;

        let mut app = ctf_app(3);
        let player = app.world_mut().spawn_empty().id();
        let (seat, pawn) = join(&mut app, player);
        {
            let mut rules = app.world_mut().resource_mut::<MatchRules>();
            rules.session.teams.switch = TeamSwitch::BetweenMatches;
            rules.session.apply(&FlowInput::OpenBetween).expect("between");
            let moved = rules
                .session
                .assign_team(
                    &SeatRequest {
                        from: Some(seat),
                        team: Some(3),
                        slot: Some(0),
                        unit: None,
                    },
                    &[],
                )
                .expect("move");
            assert_eq!(moved, vec![SeatId::new(3, 0)]);
        }
        step(&mut app);
        assert_eq!(app.world().get::<TeamId>(pawn), Some(&TeamId(3)));
        assert_eq!(app.world().get::<LedgerSlot>(pawn), Some(&LedgerSlot(0)));
        assert_eq!(
            app.world().resource::<MatchSessionAdapter>().seats[&player],
            SeatId::new(3, 0)
        );
        assert!(
            app.world().resource::<MatchRules>().session.seat_moves.is_empty(),
            "the step drained the moves"
        );
        let world = app.world_mut();
        MatchSessionRules.on_player_left(&mut RulesCtx::new(world), player);
        let session = &world.resource::<MatchRules>().session;
        assert!(session.flow.seated.is_empty(), "the leave emptied 3/0");
        assert!(world.get::<LedgerSlot>(pawn).is_none());
    }

    /// The adapter's resources reach a world snapshot and move it when they change: the seats,
    /// the spawn order of `TeamId` and the credited-death marker (finding D).
    #[test]
    fn the_adapter_state_moves_the_world_snapshot() {
        use crate::state_hash::{insert_resource_text, world_snapshot};

        let mut app = ctf_app(3);
        let player = app.world_mut().spawn_empty().id();
        let (_, pawn) = join(&mut app, player);
        step(&mut app);
        let text = |app: &App| {
            let world = app.world();
            let mut snapshot = world_snapshot(world);
            for name in ["MatchRules", "MatchSessionAdapter", "SpawnOrder<TeamId>"] {
                assert!(
                    insert_resource_text(world, &mut snapshot, name),
                    "{name} is not a reflected resource"
                );
            }
            snapshot
        };
        let before = text(&app);
        assert!(before["$resource/MatchSessionAdapter"].contains("steps: 1"));

        let second = app.world_mut().spawn_empty().id();
        let _ = join(&mut app, second);
        let joined = text(&app);
        assert_ne!(
            before["$resource/MatchSessionAdapter"], joined["$resource/MatchSessionAdapter"],
            "a seat moves the adapter text"
        );
        assert_ne!(
            before["$resource/SpawnOrder<TeamId>"], joined["$resource/SpawnOrder<TeamId>"],
            "a TeamId arrival moves the spawn-order text"
        );
        app.world_mut().entity_mut(pawn).insert(DeathCredited);
        let credited = text(&app);
        let all = |snapshot: &crate::state_hash::StateSnapshot| {
            snapshot.values().cloned().collect::<Vec<_>>().join("\n")
        };
        assert!(!all(&joined).contains("DeathCredited"));
        assert!(all(&credited).contains("DeathCredited"));
    }
}
