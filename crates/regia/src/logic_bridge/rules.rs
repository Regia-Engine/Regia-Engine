// SPDX-License-Identifier: MPL-2.0
//! The match-rules tasks and the match-event route of the logic graph (audit gap GR-7.6,
//! `docs/specs/102-logic-graph.md` § 14): designers script mode events visually ("when the flag
//! is captured, open the gate").
//!
//! Tasks (registered by `register_tasks`, sim actions: a peer without authority skips them, and a
//! client refuses them with a warning, since the match belongs to the host):
//!
//! - `rules::AddScore { points, team, seat?, entity? }`: `points` (an integer, negative takes
//!   points away) on team `team`'s board score; with `seat` too, on that seat's ledger score
//!   instead; with `entity`, on the ledger of the seat that entity holds (`TeamId` +
//!   `LedgerSlot`).
//! - `rules::EndMatch { team?, reason? }`: ends the match (with the phase machine on: the round or
//!   match at its next step) with `team` as the winner (default `0`, none) for `reason` (default
//!   `"ended"`; any `EndReason` word).
//! - `rules::SetTeam { team, entity? }`: moves `entity` (default: the graph's entity) to `team`. A
//!   seated entity changes seat through the session (its switch policy applies), then its `TeamId`
//!   and `LedgerSlot` follow; an unseated one just gets the new `TeamId`.
//! - `rules::SetSpawnEnabled { entity?, on? }`: takes a `PlayerStart` (default: the graph's
//!   entity) out of spawn selection (`on: false`) or puts it back (`on: true`, the default).
//! - `rules::LockObjective { id, on? }`: locks (`on: true`, the default) or unlocks objective
//!   `id` on the board: a locked flag takes no touches and does not capture, a locked hill scores
//!   no points.
//!
//! Each failure is a warning on the `regia::logic` target and a failed action (the graph's trace
//! shows `action_failed`).
//!
//! The route ([`route_match_events`], `PostUpdate`) posts every `MatchEvent` as the inbox
//! event `match.<kind>` (`match.flag_captured`) on each `MatchEventListener` and on the entities the
//! event involves (the unit that took or captured a flag, the flag's `ObjectiveEntity`, the seat's
//! pawn), and writes the event's fields into their blackboards first, under per-kind keys
//! `match_<kind>_<field>` ([`match_key`]).

use regia_ai::logic::LogicInbox;
use regia_ai::{BbValue, Blackboard, BtContext, BtStatus, BtTask, BtTaskRegistry, LogicGraph};
use regia_app::prelude::*;
use regia_ecs::ecs::message::MessageCursor;
use regia_framework::player::{PlayerStart, PlayerStartDisabled};
use regia_framework::rules::match_flow::SeatId;
use regia_framework::rules::objective::{EndReason, ObjectiveKind};
use regia_framework::rules::teams::SeatRequest;
use regia_framework::rules::{
    LedgerSlot, MatchEvent, MatchEventFields, MatchEventListener, MatchEventUnits, MatchRules,
    ObjectiveEntity, TeamId,
};

/// The task path of the score action.
pub const ADD_SCORE: &str = "rules::AddScore";
/// The task path of the match-end action.
pub const END_MATCH: &str = "rules::EndMatch";
/// The task path of the team-change action.
pub const SET_TEAM: &str = "rules::SetTeam";
/// The task path of the spawn-point switch.
pub const SET_SPAWN_ENABLED: &str = "rules::SetSpawnEnabled";
/// The task path of the objective lock.
pub const LOCK_OBJECTIVE: &str = "rules::LockObjective";

/// The five match-rules task paths, in palette order.
pub const RULES_TASKS: [&str; 5] = [ADD_SCORE, END_MATCH, SET_TEAM, SET_SPAWN_ENABLED, LOCK_OBJECTIVE];

/// The fields the match-event route writes, as the suffix of the key `match_<kind>_<field>`
/// ([`match_key`]): team, seat slot, score, objective id and round are integers, unit an entity,
/// reason a string, tick an integer.
pub const MATCH_EVENT_FIELDS: [&str; 8] = [
    "team",
    "seat",
    "score",
    "objective",
    "unit",
    "reason",
    "round",
    "tick",
];

/// The blackboard key the match-event route writes for `field` of an event of `kind`:
/// `match_<kind>_<field>` (`match_flag_captured_team`). Keys are per kind, so a rule on
/// `match.flag_captured` reads the capture's fields even when the same frame also sent a
/// `match.player_scored`.
pub fn match_key(kind: &str, field: &str) -> String {
    format!("match_{kind}_{field}")
}

pub(super) fn register(registry: &mut BtTaskRegistry) {
    registry.register_factory(ADD_SCORE, || Box::new(RulesTask::new(RulesOp::AddScore)));
    registry.register_factory(END_MATCH, || Box::new(RulesTask::new(RulesOp::EndMatch)));
    registry.register_factory(SET_TEAM, || Box::new(RulesTask::new(RulesOp::SetTeam)));
    registry.register_factory(SET_SPAWN_ENABLED, || {
        Box::new(RulesTask::new(RulesOp::SetSpawnEnabled))
    });
    registry.register_factory(LOCK_OBJECTIVE, || {
        Box::new(RulesTask::new(RulesOp::LockObjective))
    });
}

/// True (with a warning) when this process is a network client: the match belongs to the host, so
/// a rules task changes nothing there. Offline, listen and dedicated servers run them.
pub fn rules_refused_here(task: &str, world: &World) -> bool {
    #[cfg(feature = "net")]
    if world
        .get_resource::<regia_net::NetMode>()
        .is_some_and(|mode| mode.is_client())
    {
        tracing::warn!(
            target: "regia::logic",
            task,
            "{task} refused on a client: the match belongs to the host, so rules tasks run there only"
        );
        return true;
    }
    #[cfg(not(feature = "net"))]
    let _ = (task, world);
    false
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RulesOp {
    AddScore,
    EndMatch,
    SetTeam,
    SetSpawnEnabled,
    LockObjective,
}

impl RulesOp {
    fn path(self) -> &'static str {
        match self {
            Self::AddScore => ADD_SCORE,
            Self::EndMatch => END_MATCH,
            Self::SetTeam => SET_TEAM,
            Self::SetSpawnEnabled => SET_SPAWN_ENABLED,
            Self::LockObjective => LOCK_OBJECTIVE,
        }
    }
}

/// One `rules::*` action (module docs).
struct RulesTask {
    op: RulesOp,
    params: Vec<(String, BbValue)>,
}

impl RulesTask {
    fn new(op: RulesOp) -> Self {
        Self {
            op,
            params: Vec::new(),
        }
    }

    fn get(&self, key: &str) -> Option<&BbValue> {
        self.params.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// An integer parameter; `Err` names a parameter of the wrong type.
    fn int(&self, key: &str) -> Result<Option<i64>, String> {
        match self.get(key) {
            None => Ok(None),
            Some(BbValue::Int(i)) => Ok(Some(*i)),
            Some(_) => Err(format!("`{key}` must be an integer")),
        }
    }

    /// A small integer parameter (a team, a seat slot).
    fn byte(&self, key: &str) -> Result<Option<u8>, String> {
        match self.int(key)? {
            None => Ok(None),
            Some(i) => u8::try_from(i)
                .map(Some)
                .map_err(|_| format!("`{key}` must be 0..=255, not {i}")),
        }
    }

    fn flag(&self, key: &str, default: bool) -> Result<bool, String> {
        match self.get(key) {
            None => Ok(default),
            Some(BbValue::Bool(b)) => Ok(*b),
            Some(_) => Err(format!("`{key}` must be a bool")),
        }
    }

    /// An entity parameter (default: the graph's entity); it must be alive.
    fn entity(&self, key: &str, ctx: &BtContext<'_>) -> Result<Entity, String> {
        let entity = match self.get(key) {
            None => ctx.entity,
            Some(BbValue::Entity(bits)) => {
                Entity::try_from_bits(*bits).ok_or_else(|| format!("`{key}` is not an entity"))?
            }
            Some(_) => return Err(format!("`{key}` must be an entity")),
        };
        if ctx.world.get_entity(entity).is_err() {
            return Err(format!("`{key}` names an entity that is gone"));
        }
        Ok(entity)
    }

    fn run(&self, ctx: &mut BtContext<'_>) -> Result<(), String> {
        match self.op {
            RulesOp::AddScore => self.add_score(ctx),
            RulesOp::EndMatch => self.end_match(ctx),
            RulesOp::SetTeam => self.set_team(ctx),
            RulesOp::SetSpawnEnabled => self.set_spawn_enabled(ctx),
            RulesOp::LockObjective => self.lock_objective(ctx),
        }
    }

    fn add_score(&self, ctx: &mut BtContext<'_>) -> Result<(), String> {
        let points = self.int("points")?.ok_or("needs `points`")?;
        let seat = if self.get("entity").is_some() {
            let entity = self.entity("entity", ctx)?;
            let seat = seat_of(ctx.world, entity).ok_or("`entity` holds no seat (TeamId and LedgerSlot)")?;
            Some(seat)
        } else {
            let team = self.byte("team")?.ok_or("needs `team` (or `entity`)")?;
            match self.byte("seat")? {
                Some(slot) => Some(SeatId::new(team, slot)),
                None => {
                    let mut rules = ctx
                        .world
                        .get_resource_mut::<MatchRules>()
                        .ok_or("no MatchRules")?;
                    rules
                        .session
                        .add_team_score(team, points)
                        .ok_or_else(|| format!("team {team} is not a fighting team, or the match is over"))?;
                    None
                }
            }
        };
        if let Some(seat) = seat {
            let mut rules = ctx
                .world
                .get_resource_mut::<MatchRules>()
                .ok_or("no MatchRules")?;
            rules.session.add_seat_score(seat, points).ok_or_else(|| {
                format!(
                    "nobody holds seat {}/{}, or the match is over",
                    seat.team, seat.slot
                )
            })?;
        }
        Ok(())
    }

    fn end_match(&self, ctx: &mut BtContext<'_>) -> Result<(), String> {
        let team = self.byte("team")?.unwrap_or(0);
        let reason = match self.get("reason") {
            None => EndReason::Ended,
            Some(BbValue::String(word)) => end_reason(word).ok_or_else(|| {
                format!(
                    "unknown reason {word:?} (ended, forfeit, draw, time_limit, rounds_won, capture_limit_reached, hill_score_limit_reached)"
                )
            })?,
            Some(_) => return Err("`reason` must be a string".to_string()),
        };
        let mut rules = ctx
            .world
            .get_resource_mut::<MatchRules>()
            .ok_or("no MatchRules")?;
        if rules.session.board.match_over {
            return Err("the match is already over".to_string());
        }
        rules.session.report_condition(team, reason);
        Ok(())
    }

    fn set_team(&self, ctx: &mut BtContext<'_>) -> Result<(), String> {
        let team = self.byte("team")?.ok_or("needs `team`")?;
        let entity = self.entity("entity", ctx)?;
        let seated = seat_of(ctx.world, entity).filter(|seat| {
            ctx.world
                .get_resource::<MatchRules>()
                .is_some_and(|rules| rules.session.flow.seated.contains(seat))
        });
        match seated {
            Some(seat) if seat.team == team => Ok(()),
            Some(seat) => {
                let request = SeatRequest {
                    from: Some(seat),
                    team: Some(team),
                    ..SeatRequest::default()
                };
                let mut rules = ctx
                    .world
                    .get_resource_mut::<MatchRules>()
                    .ok_or("no MatchRules")?;
                rules
                    .session
                    .assign_team(&request, &[])
                    .map_err(|error| format!("{} ({error})", error.code()))?;
                // TeamId and LedgerSlot follow the move on every entity that held the seat.
                let _ = regia_framework::rules::retag_moved_seats(ctx.world);
                Ok(())
            }
            None => {
                let mut e = ctx
                    .world
                    .get_entity_mut(entity)
                    .map_err(|_| "the entity is gone".to_string())?;
                e.insert(TeamId(team));
                Ok(())
            }
        }
    }

    fn set_spawn_enabled(&self, ctx: &mut BtContext<'_>) -> Result<(), String> {
        let on = self.flag("on", true)?;
        let entity = self.entity("entity", ctx)?;
        let mut e = ctx
            .world
            .get_entity_mut(entity)
            .map_err(|_| "the entity is gone".to_string())?;
        if !e.contains::<PlayerStart>() {
            return Err("`entity` is not a PlayerStart".to_string());
        }
        if on {
            e.remove::<PlayerStartDisabled>();
        } else if !e.contains::<PlayerStartDisabled>() {
            e.insert(PlayerStartDisabled);
        }
        Ok(())
    }

    fn lock_objective(&self, ctx: &mut BtContext<'_>) -> Result<(), String> {
        let id = self.int("id")?.ok_or("needs `id`")?;
        let id = u32::try_from(id).map_err(|_| format!("`id` must be 0..=4294967295, not {id}"))?;
        let on = self.flag("on", true)?;
        let mut rules = ctx
            .world
            .get_resource_mut::<MatchRules>()
            .ok_or("no MatchRules")?;
        if rules.session.board.set_locked(id, on) {
            Ok(())
        } else {
            Err(format!("the board has no objective {id}"))
        }
    }
}

impl BtTask for RulesTask {
    fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
        let path = self.op.path();
        if rules_refused_here(path, ctx.world) {
            return BtStatus::Failure;
        }
        match self.run(ctx) {
            Ok(()) => BtStatus::Success,
            Err(detail) => {
                tracing::warn!(target: "regia::logic", task = path, "{path}: {detail}");
                BtStatus::Failure
            }
        }
    }

    fn logic_params(&mut self, params: &[(String, BbValue)]) {
        self.params.clear();
        self.params.extend_from_slice(params);
    }
}

/// The seat `entity` holds: its `TeamId` (a fighting team, not `0`) and its `LedgerSlot`.
fn seat_of(world: &World, entity: Entity) -> Option<SeatId> {
    let team = world.get::<TeamId>(entity)?.0;
    let slot = world.get::<LedgerSlot>(entity)?.0;
    (team != 0).then(|| SeatId::new(team, slot))
}

/// The `EndReason` a word names (`EndReason::as_str`).
fn end_reason(word: &str) -> Option<EndReason> {
    [
        EndReason::Ended,
        EndReason::Forfeit,
        EndReason::Draw,
        EndReason::TimeLimit,
        EndReason::RoundsWon,
        EndReason::CaptureLimitReached,
        EndReason::HillScoreLimitReached,
    ]
    .into_iter()
    .find(|reason| reason.as_str() == word)
}

// ---- match events ------------------------------------------------------------------------------------------

/// `PostUpdate` (once per frame, after the frame's fixed steps stepped the session): every
/// `MatchEvent` since the last run goes to the logic graphs that care, as the inbox event `match.<kind>`:
///
/// - every entity marked `MatchEventListener`;
/// - the entity the event involves: the unit that took, dropped, returned or captured a flag (the
///   entity the session adapter recorded for that contestant on the step the event happened,
///   `MatchEventUnits`, and only while that same entity is alive: an entity that reused the
///   departed unit's index gets nothing), the flag's
///   `ObjectiveEntity` (or the hill's, for `match.hill_point`; a capture zone's or a race
///   checkpoint's for their events), and the pawn that holds the seat of a seat event
///   (`match.player_scored`, `match.seat_left`, `match.respawn_at`, `match.eliminated`, the
///   `match.racer_*` events and `match.surrender_vote`; ADR-8731).
///
/// Before the event lands, its fields are written into the target's blackboard under per-kind keys
/// (`match_<kind>_<field>`, [`match_key`], fields [`MATCH_EVENT_FIELDS`]: `match_flag_captured_team`,
/// `match_flag_captured_unit`, ...); a field the event does not carry is removed, so a key never
/// shows another event's value. Graphs take the events on their next tick. When one frame sends
/// several events of one kind to one entity, that kind's keys hold the last one's fields.
/// Entities without a `LogicGraph` are skipped.
pub fn route_match_events(world: &mut World, mut cursor: Local<MessageCursor<MatchEvent>>) {
    let Some(messages) = world.get_resource::<Messages<MatchEvent>>() else {
        return;
    };
    let events: Vec<(MatchEvent, usize)> = cursor
        .read_with_id(messages)
        .map(|(event, id)| (event.clone(), id.id))
        .collect();
    if events.is_empty() {
        return;
    }
    let _span =
        tracing::debug_span!(target: "regia::logic", "route_match_events", events = events.len()).entered();
    let mut hosts: Vec<Entity> = world
        .query_filtered::<Entity, (With<MatchEventListener>, With<LogicGraph>)>()
        .iter(world)
        .collect();
    hosts.sort();
    let hill = world.get_resource::<MatchRules>().and_then(|rules| {
        rules
            .session
            .board
            .objectives
            .iter()
            .find(|o| o.kind == ObjectiveKind::Hill)
            .map(|o| o.entity_id)
    });
    for (event, id) in events {
        let name = event.logic_name();
        let mut fields = event.fields();
        if fields.kind == "hill_point" {
            fields.objective = hill;
        }
        let unit = fields.unit.and_then(|_| unit_entity(world, id));
        let mut targets = hosts.clone();
        for involved in involved_entities(world, &fields, unit) {
            if !targets.contains(&involved) && world.get::<LogicGraph>(involved).is_some() {
                targets.push(involved);
            }
        }
        for target in targets {
            deliver(world, target, &name, &fields, unit);
        }
    }
}

/// The unit entity of message `id`: the entity the adapter recorded when it sent the event, while
/// it was that contestant, and only while that same entity (index and generation) is alive. The
/// contestant id alone is an entity index, which a later spawn can reuse.
fn unit_entity(world: &World, id: usize) -> Option<Entity> {
    world
        .get_resource::<MatchEventUnits>()
        .and_then(|units| units.unit(id))
        .filter(|unit| world.get_entity(*unit).is_ok())
}

/// The entities a match event is about, in a fixed order: the unit, the objective, the seat.
fn involved_entities(world: &mut World, fields: &MatchEventFields, unit: Option<Entity>) -> Vec<Entity> {
    let mut out = Vec::new();
    if let Some(unit) = unit {
        out.push(unit);
    }
    if let Some(id) = fields.objective {
        let mut tagged: Vec<Entity> = world
            .query::<(Entity, &ObjectiveEntity)>()
            .iter(world)
            .filter(|(_, tag)| tag.0 == id)
            .map(|(e, _)| e)
            .collect();
        tagged.sort();
        out.extend(tagged);
    }
    if let (Some(team), Some(slot), true) = (
        fields.team,
        fields.seat,
        matches!(
            fields.kind,
            "player_scored"
                | "seat_left"
                | "respawn_at"
                | "eliminated"
                | "racer_checkpoint"
                | "racer_lap"
                | "racer_missed_checkpoint"
                | "racer_wrong_way"
                | "racer_finished"
                | "racer_dnf"
                | "surrender_vote"
        ),
    ) {
        let mut seated: Vec<Entity> = world
            .query::<(Entity, &TeamId, &LedgerSlot)>()
            .iter(world)
            .filter(|(_, t, s)| t.0 == team && s.0 == slot)
            .map(|(e, ..)| e)
            .collect();
        seated.sort();
        out.extend(seated);
    }
    out
}

fn deliver(world: &mut World, target: Entity, name: &str, fields: &MatchEventFields, unit: Option<Entity>) {
    let Ok(mut entity) = world.get_entity_mut(target) else {
        return;
    };
    if !entity.contains::<Blackboard>() {
        entity.insert(Blackboard::new());
    }
    if let Some(mut bb) = entity.get_mut::<Blackboard>() {
        write_fields(&mut bb, fields, unit);
    }
    match entity.get_mut::<LogicInbox>() {
        Some(mut inbox) => inbox.events.push(name.to_string()),
        None => {
            entity.insert(LogicInbox {
                events: vec![name.to_string()],
            });
        }
    }
}

fn write_fields(bb: &mut Blackboard, f: &MatchEventFields, unit: Option<Entity>) {
    let values: [Option<BbValue>; 8] = [
        f.team.map(|t| BbValue::Int(i64::from(t))),
        f.seat.map(|s| BbValue::Int(i64::from(s))),
        f.score.map(BbValue::Int),
        f.objective.map(|o| BbValue::Int(i64::from(o))),
        unit.map(|e| BbValue::Entity(e.to_bits())),
        f.reason.clone().map(BbValue::String),
        f.round.map(|r| BbValue::Int(i64::from(r))),
        Some(BbValue::Int(i64::try_from(f.tick).unwrap_or(i64::MAX))),
    ];
    for (field, value) in MATCH_EVENT_FIELDS.into_iter().zip(values) {
        let key = match_key(f.kind, field);
        match value {
            Some(value) => {
                bb.insert(key, value);
            }
            None => {
                bb.remove(&key);
            }
        }
    }
}
