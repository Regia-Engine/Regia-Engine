// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface of the match rules core (ADR-8102, `book/agents/match-rules.md`):
//! `rules.match_get`, `rules.match_setup`, `rules.match_input`, `rules.match_objective`,
//! `rules.match_step` and `rules.match_assign` over the [`MatchRules`] resource.
//!
//! Every method but `rules.match_setup` answers `match_rules_unavailable` when the app has no
//! [`MatchRules`] (add `regia_framework::rules::MatchRulesPlugin`, or call `rules.match_setup`,
//! which inserts it). A refused flow input or objective answers the rules' own code (`match_*`,
//! [`RulesError::code`]) with `data.reason` holding the same code; a malformed parameter answers
//! `F0002`.

use regia_app::prelude::*;
use regia_framework::rules::ctf::DropCheck;
use regia_framework::rules::end_stats::{self as ledger, PlayerEndStats};
use regia_framework::rules::match_flow::{FlowInput, Issuer, SeatId};
use regia_framework::rules::objective::{Contestant, HillVolume};
use regia_framework::rules::phases::PhaseSettings;
use regia_framework::rules::playlist::{MAX_ROW_WEIGHT, PlaylistEntry, PlaylistOrder};
use regia_framework::rules::series::{self as series_mod, RoundResult, SERIES_TEAM_SLOTS, SeriesState};
use regia_framework::rules::session::{MatchSession, SessionSettings};
use regia_framework::rules::teams::SeatRequest;
use regia_framework::rules::win_condition::{self as wins, StockRule};
use regia_framework::rules::zones::{Contest, ZoneMode, ZoneSettings};
use regia_framework::rules::{MatchRules, RulesError};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::params::check_known_and_types;
use crate::protocol::{self, RpcError};

/// Most steps one `rules.match_step` runs.
pub const MAX_STEPS: u64 = 100_000;

/// Most units one `rules.match_step` takes.
pub const MAX_UNITS: usize = 4096;

/// Most inputs one `rules.match_input` applies.
pub const MAX_INPUTS: usize = 1024;

/// Most playlist rows one `rules.match_setup` takes.
pub const MAX_ROWS: usize = 1024;

/// Most members one `rules.match_assign` group holds, including the first seat.
pub const MAX_ASSIGN_GROUP: usize = 64;

fn unavailable() -> RpcError {
    RpcError::new(
        protocol::UNSUPPORTED,
        "match_rules_unavailable",
        "no MatchRules resource in this app",
        "add regia_framework::rules::MatchRulesPlugin, or call rules.match_setup",
    )
}

fn invalid(message: impl Into<String>) -> RpcError {
    RpcError::invalid_params(message)
}

/// A refused input or objective: the rules' code, also under `data.reason`.
fn refused(error: &RulesError) -> RpcError {
    let mut out = RpcError::new(
        protocol::INVALID_PARAMS,
        error.code(),
        error.to_string(),
        "rules.match_get shows the flow and the board; book/agents/match-rules.md lists the codes",
    );
    if let Some(Value::Object(data)) = out.data.as_mut() {
        data.insert("reason".into(), json!(error.code()));
    }
    out
}

fn session_json(session: &MatchSession) -> Result<Value, RpcError> {
    let mut out = serde_json::to_value(session)
        .map_err(|e| RpcError::internal(format!("serializing the session: {e}")))?;
    let (ready, need) = session.flow.ready_counts();
    if let Value::Object(map) = &mut out {
        // Series, vote, ledger and the win rule have their own methods. The match_* result
        // objects stay the shape those methods already publish.
        for key in [
            "series",
            "vote",
            "ledger",
            "maps",
            "carry",
            "player_count",
            "win_rule",
            // ADR-8647: respawn settings, the respawn book and the spawn picker's state are read
            // through rules.spawn_preview.
            "respawn",
            "respawns",
            "spawn",
            // GR-11.3/11.4: the zone, the placements and the victory state are read through
            // rules.match_get's `zone`, `placements` and `victory` keys.
            "zone",
            "zone_state",
            "elimination",
            "placements",
            "victory",
            "victory_state",
            // ADR-8897: the host's content list is a host input, not match state; the
            // `row_skipped` events say what it did.
            "missing_content",
            // ADR-8652: the match-end record is read through rules.match_results.
            "launched_step",
            "match_end",
            // ADR-8881: the serialized shape version and the vacated coordinates are session
            // bookkeeping; the match_* result objects keep the shape their schemas publish.
            "version",
            "vacated",
        ] {
            map.remove(key);
        }
        map.insert("ready_counts".into(), json!({ "ready": ready, "need": need }));
        map.insert("mode".into(), json!(session.board.mode.as_lobby_str()));
        // ADR-8485: the phase, the round and the clock, read without walking `flow.phases`.
        let phases = &session.flow.phases;
        let settings = &session.flow.settings.phases;
        map.insert("phase".into(), json!(phases.phase.as_str()));
        map.insert("round".into(), json!(phases.round));
        map.insert("time_left_ticks".into(), json!(phases.time_left(settings)));
        map.insert("round_wins".into(), json!(phases.round_wins));
        // ADR-8695: co-op waves and races appear only when their settings are on.
        if session.waves.is_off() {
            map.remove("waves");
        } else if let Some(Value::Object(waves)) = map.get_mut("waves") {
            let state = &session.waves.state;
            waves.insert("alive".into(), json!(state.alive_total()));
            waves.insert("cleared".into(), json!(state.cleared()));
            waves.insert("total".into(), json!(session.waves.settings.waves.len()));
        }
        if session.race.is_off() {
            map.remove("race");
        } else if let Some(Value::Object(race)) = map.get_mut("race") {
            let standings = serde_json::to_value(session.race_standings())
                .map_err(|e| RpcError::internal(format!("serializing the standings: {e}")))?;
            race.insert("standings".into(), standings);
        }
    }
    Ok(out)
}

fn session_mut(world: &mut World) -> Result<Mut<'_, MatchRules>, RpcError> {
    world.get_resource_mut::<MatchRules>().ok_or_else(unavailable)
}

/// `rules.match_get`: the whole session (flow, playlist, board, current row, teams, ledgers) plus
/// `ready_counts`, the board's `mode` lobby id, and the `phase`, `round`, `time_left_ticks` and
/// `round_wins` of the phase machine (ADR-8485).
///
/// A client without a session answers from the replicated views instead (`source: "replicated"`,
/// [`replicated_json`]); without either it is `match_rules_unavailable`.
pub fn match_get(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    check_known_and_types("rules.match_get", params, &[])?;
    if let Some(rules) = world.get_resource::<MatchRules>() {
        let mut out = session_json(&rules.session)?;
        if let Value::Object(map) = &mut out {
            let session = &rules.session;
            map.insert("zone".into(), zone_json(session));
            map.insert("placements".into(), json!(session.placements()));
            map.insert(
                "victory".into(),
                json!({
                    "conditions": session.victory.conditions,
                    "surrender": session.victory.surrender,
                    "alliances": session.elimination.alliances,
                    "last_standing": session.elimination.last_standing,
                    "contenders": session.placements.contenders,
                    "out": session.placements.out,
                    "winners": session.placements.winners,
                    "decided": session.placements.decided,
                    "state": session.victory_state,
                }),
            );
        }
        return Ok(out);
    }
    replicated_json(world).ok_or_else(unavailable)
}

/// The match as a client sees it (`book/agents/match-rules.md`, "Replicated match state"): the
/// views of the match-state entity, every `PlayerScoreView` (with `local` for this machine's
/// player) and the replication counters. `None` when the world holds no match-state entity.
pub fn replicated_json(world: &mut World) -> Option<Value> {
    use regia_framework::rules::match_replication::{ObjectiveViewState, match_views, player_score_views};
    let (phase, teams, objectives) = match_views(world)?;
    let net_tick = world
        .get_resource::<regia_net::runtime::NetNode>()
        .map_or(phase.net_tick, |node| node.tick.0);
    let counters = world
        .get_resource::<regia_framework::rules::MatchStateCounters>()
        .copied()
        .unwrap_or_default();
    let players: Vec<Value> = player_score_views(world)
        .into_iter()
        .map(|(v, local)| {
            json!({
                "team": v.team,
                "slot": v.slot,
                "row_id": v.row_id,
                "kills": v.kills,
                "deaths": v.deaths,
                "assists": v.assists,
                "objective_points": v.objective_points,
                "score": v.score,
                "local": local,
            })
        })
        .collect();
    let objectives: Vec<Value> = objectives
        .items
        .iter()
        .map(|o| {
            let (state, carrier, seat) = match o.state {
                ObjectiveViewState::AtBase => ("at_base", None, None),
                ObjectiveViewState::Carried { carrier, seat } => ("carried", Some(carrier), seat),
                ObjectiveViewState::Dropped => ("dropped", None, None),
                ObjectiveViewState::Hill => ("hill", None, None),
                ObjectiveViewState::Zone { .. } => ("zone", None, None),
            };
            let mut view = json!({
                "id": o.id,
                "kind": o.kind().as_str(),
                "team": o.team,
                "state": state,
                "carrier": carrier,
                "carrier_seat": seat.map(|s| json!({ "team": s.team, "slot": s.slot })),
                "position": [o.position.x, o.position.y, o.position.z],
                "capture_progress": o.capture_progress,
            });
            // A capture zone also names the team filling its bar and its lock (ADR-8731).
            if let (
                ObjectiveViewState::Zone {
                    capturing, locked, ..
                },
                Value::Object(map),
            ) = (o.state, &mut view)
            {
                map.insert("capturing".into(), json!(capturing));
                map.insert("locked".into(), json!(locked));
            }
            view
        })
        .collect();
    let labels: Vec<Value> = teams
        .labels
        .iter()
        .map(|l| json!({ "id": l.id, "name": l.name, "color": l.color.to_be_bytes() }))
        .collect();
    Some(json!({
        "source": "replicated",
        "phase": phase.phase().as_str(),
        "round": phase.round,
        "match_tick": phase.match_tick_at(net_tick),
        "time_left_ticks": phase.time_left_at(net_tick),
        "paused": phase.paused,
        "paused_by": phase.paused_by,
        "pauser": phase.pauser.map(|s| json!({ "team": s.team, "slot": s.slot })),
        "between": phase.between,
        "starts_at": phase.starts_at,
        "match_over": phase.match_over,
        "winner": phase.winner,
        "end_reason": phase.end_reason(),
        "team_scores": teams.scores,
        "round_wins": teams.round_wins,
        "teams": labels,
        "objectives": objectives,
        "players": players,
        "replication": {
            "events_received": counters.events_received,
            "events_oversized": counters.events_oversized,
            "events_malformed": counters.events_malformed,
        },
    }))
}

/// `rules.match_get`'s `zone`: `null` without a schedule, else the stage, the phase, the current
/// and target circles (XZ, world units) and the damage per tick outside.
fn zone_json(session: &MatchSession) -> Value {
    let Some(schedule) = &session.zone else {
        return Value::Null;
    };
    let state = &session.zone_state;
    json!({
        "stage": state.stage.as_str(),
        "phase": state.phase,
        "phases": schedule.phases.len(),
        "started_at": state.started_at,
        "stage_since": state.stage_since,
        "center": state.current.center(),
        "radius": state.current.radius(),
        "target_center": state.to.center(),
        "target_radius": state.to.radius(),
        "damage_per_tick": state.damage(schedule),
        "schedule": schedule,
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RowParam {
    map_id: String,
    mode: String,
    #[serde(default)]
    options: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    weight: Option<u32>,
    #[serde(default)]
    min_players: Option<u32>,
    #[serde(default)]
    max_players: Option<u32>,
}

/// `rules.match_setup`: replace the session with a new one (`settings`, `playlist`, `looping`),
/// inserting [`MatchRules`] when absent; `start: true` starts the first row at once, otherwise
/// `between: true` opens the ready gate. `phases` (ADR-8485) replaces `settings.flow.phases`;
/// phase settings that cannot run answer `match_phase_settings`. `ruleset` (ADR-8694) takes the
/// settings, mode, win rule and zone rules from a loaded rule set resolved with `options` (it
/// excludes `settings` and `phases`); the answer then adds `ruleset` (the resolved rules and their
/// content hash). Rows whose `mode` names a rule set launch on its skeleton. `order` (`in_order`,
/// `{ shuffle: { seed } }`, `{ weighted: { seed } }`), `no_repeat_last`, `playlist_id` and the
/// rows' `weight`, `min_players` and `max_players` set the rotation (ADR-8897).
pub fn match_setup(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    check_known_and_types("rules.match_setup", params, &[])?;
    let row_list: Vec<RowParam> = match params.get("playlist").filter(|v| !v.is_null()) {
        Some(v) => serde_json::from_value(v.clone()).map_err(|e| invalid(format!("`playlist`: {e}")))?,
        None => Vec::new(),
    };
    let row_entries: Vec<PlaylistEntry> = row_list
        .into_iter()
        .map(|r| PlaylistEntry {
            map_id: r.map_id,
            mode: r.mode,
            options: r.options,
            weight: r.weight.unwrap_or(1),
            min_players: r.min_players,
            max_players: r.max_players,
        })
        .collect();
    if row_entries.len() > MAX_ROWS {
        return Err(invalid(format!("`playlist` holds at most {MAX_ROWS} rows")));
    }
    for (row, entry) in row_entries.iter().enumerate() {
        if entry.weight > MAX_ROW_WEIGHT {
            return Err(invalid(format!(
                "`playlist[{row}].weight` is at most {MAX_ROW_WEIGHT}"
            )));
        }
        // Each bound as the rotation's gate reads it: the field, else the option of that name.
        if let (Some(min), Some(max)) = (entry.min_players_bound(), entry.max_players_bound())
            && min > max
        {
            return Err(invalid(format!(
                "`playlist[{row}]`: min_players {min} is above max_players {max}"
            )));
        }
    }
    let order: PlaylistOrder = match params.get("order").filter(|v| !v.is_null()) {
        Some(v) => serde_json::from_value(v.clone()).map_err(|e| invalid(format!("`order`: {e}")))?,
        None => PlaylistOrder::InOrder,
    };
    let no_repeat_last = match crate::params::opt_u64(params, "no_repeat_last")? {
        Some(n) => u8::try_from(n).map_err(|_| invalid("`no_repeat_last` is at most 255"))?,
        None => 0,
    };
    let playlist_id = crate::params::opt_str(params, "playlist_id")?
        .map(str::to_owned)
        .unwrap_or_default();
    let (resolved, rule_modes) = crate::ruleset::setup_rules(world, params, &row_entries)?;
    let mut settings: SessionSettings = match params.get("settings").filter(|v| !v.is_null()) {
        Some(v) => serde_json::from_value(v.clone()).map_err(|e| invalid(format!("`settings`: {e}")))?,
        None => SessionSettings::default(),
    };
    if let Some(v) = params.get("phases").filter(|v| !v.is_null()) {
        settings.flow.phases = serde_json::from_value::<PhaseSettings>(v.clone())
            .map_err(|e| invalid(format!("`phases`: {e}")))?;
    }
    settings.flow.phases.validate().map_err(|e| refused(&e))?;
    settings.waves.validate().map_err(|e| refused(&e))?;
    settings.race.validate().map_err(|e| refused(&e))?;
    if let Some(zone) = &settings.zone {
        zone.validate().map_err(|e| refused(&e))?;
    }
    settings.victory.validate().map_err(|e| refused(&e))?;
    let start = crate::params::opt_bool(params, "start")?.unwrap_or(false);
    let between = crate::params::opt_bool(params, "between")?.unwrap_or(false);
    if start && between {
        return Err(invalid("`start` and `between` exclude each other"));
    }
    let mut session = match &resolved {
        Some(rules) => rules.session(),
        None => MatchSession::new(settings),
    };
    session.rule_modes = rule_modes;
    session.playlist.looping = crate::params::opt_bool(params, "looping")?.unwrap_or(false);
    session.playlist.order = order;
    session.playlist.no_repeat_last = no_repeat_last;
    session.playlist.id = playlist_id;
    session.playlist.set(row_entries);
    if start {
        session.start_now();
    } else if between {
        session.flow.open_between();
    }
    let mut out = session_json(&session)?;
    if let (Some(rules), Value::Object(map)) = (&resolved, &mut out) {
        map.insert("ruleset".into(), crate::ruleset::resolved_json(rules)?);
    }
    world.insert_resource(MatchRules { session });
    Ok(out)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AsSeat {
    team: u8,
    slot: u8,
}

/// `as` omitted is the host. A present object is the guest on that seat.
fn issuer_of(params: &Value) -> Result<Issuer, RpcError> {
    match params.get("as").filter(|value| !value.is_null()) {
        None => Ok(Issuer::host()),
        Some(value) => {
            let seat: AsSeat =
                serde_json::from_value(value.clone()).map_err(|e| invalid(format!("`as`: {e}")))?;
            Ok(Issuer::guest(SeatId::new(seat.team, seat.slot)))
        }
    }
}

/// `rules.match_input`: apply `input` (one flow input) or `inputs` (several, in order) on the
/// current step. Stops at the first refused input; the ones before it stay applied.
///
/// `as` is `{ team, slot }` for a guest issuer. Omitted, the caller is the host. The issuer is
/// not read from the input.
pub fn match_input(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    check_known_and_types("rules.match_input", params, &[])?;
    let issuer = issuer_of(params)?;
    let mut inputs: Vec<FlowInput> = Vec::new();
    if let Some(v) = params.get("input").filter(|v| !v.is_null()) {
        inputs.push(serde_json::from_value(v.clone()).map_err(|e| invalid(format!("`input`: {e}")))?);
    }
    if let Some(v) = params.get("inputs").filter(|v| !v.is_null()) {
        let many: Vec<FlowInput> =
            serde_json::from_value(v.clone()).map_err(|e| invalid(format!("`inputs`: {e}")))?;
        inputs.extend(many);
    }
    if inputs.is_empty() {
        return Err(invalid("give `input` or `inputs`"));
    }
    if inputs.len() > MAX_INPUTS {
        return Err(invalid(format!("at most {MAX_INPUTS} inputs per call")));
    }
    let mut rules = session_mut(world)?;
    let session = &mut rules.session;
    for input in &inputs {
        session.apply_from(issuer, input).map_err(|e| refused(&e))?;
    }
    let events = session.drain_events();
    let mut out = session_json(session)?;
    if let Value::Object(map) = &mut out {
        map.insert("applied".into(), json!(inputs.len()));
        map.insert("events".into(), json!(events));
    }
    Ok(out)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FlagParam {
    team: u8,
    #[serde(default)]
    id: Option<u32>,
    #[serde(default)]
    position: Option<[f32; 3]>,
    #[serde(default)]
    offset: Option<f32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HillParam {
    #[serde(default)]
    id: Option<u32>,
    min: [f32; 3],
    max: [f32; 3],
}

/// `zone`: a capture zone (ADR-8649). Missing fields keep [`ZoneSettings::new`]'s defaults;
/// `owner_scores: false` turns owner scoring off.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ZoneParam {
    #[serde(default)]
    id: Option<u32>,
    min: [f32; 3],
    max: [f32; 3],
    #[serde(default)]
    capture_ticks: Option<u64>,
    #[serde(default)]
    decay_ticks: Option<u64>,
    #[serde(default)]
    contest: Option<Contest>,
    #[serde(default)]
    max_rate_units: Option<u32>,
    #[serde(default)]
    owner_scores_every: Option<u32>,
    #[serde(default = "yes")]
    owner_scores: bool,
    #[serde(default)]
    locked: bool,
}

impl ZoneParam {
    fn settings(&self) -> ZoneSettings {
        let base = ZoneSettings::new(HillVolume {
            min: self.min,
            max: self.max,
        });
        ZoneSettings {
            capture_ticks: self.capture_ticks.unwrap_or(base.capture_ticks),
            decay_ticks: self.decay_ticks,
            contest: self.contest.unwrap_or(base.contest),
            max_rate_units: self.max_rate_units.unwrap_or(base.max_rate_units),
            owner_scores_every: if self.owner_scores {
                self.owner_scores_every.or(base.owner_scores_every)
            } else {
                None
            },
            locked: self.locked,
            ..base
        }
    }
}

/// `zone_rules`: how the zones play together (ADR-8649).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ZoneRulesParam {
    mode: String,
    #[serde(default)]
    rotation: Option<Vec<u32>>,
    #[serde(default)]
    rotate_ticks: Option<u64>,
    #[serde(default)]
    attackers: Option<u8>,
    #[serde(default)]
    defenders: Option<u8>,
    #[serde(default)]
    order: Option<Vec<u32>>,
    #[serde(default)]
    score_limit: Option<u32>,
}

impl ZoneRulesParam {
    fn mode(self) -> Result<(ZoneMode, Option<u32>), RpcError> {
        let need = |field: &str| invalid(format!("`zone_rules`: mode `{}` needs `{field}`", self.mode));
        let mode = match self.mode.as_str() {
            "domination" => ZoneMode::Domination,
            "hardpoint" => ZoneMode::Hardpoint {
                rotation: self.rotation.clone().ok_or_else(|| need("rotation"))?,
                rotate_ticks: self.rotate_ticks.ok_or_else(|| need("rotate_ticks"))?,
            },
            "attack_defend" => ZoneMode::AttackDefend {
                attackers: self.attackers.ok_or_else(|| need("attackers"))?,
                defenders: self.defenders.ok_or_else(|| need("defenders"))?,
                order: self.order.clone().ok_or_else(|| need("order"))?,
            },
            other => {
                return Err(invalid(format!(
                    "`zone_rules`: unknown mode `{other}` (domination, hardpoint, attack_defend)"
                )));
            }
        };
        Ok((mode, self.score_limit))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlaceFlagsParam {
    teams: Vec<u8>,
    offset: f32,
    #[serde(default)]
    first_id: Option<u32>,
}

/// The first objective id at or above `from` that the board does not use.
fn free_id(session: &MatchSession, from: u32) -> u32 {
    let mut id = from;
    while session.board.objective(id).is_some() {
        id = id.wrapping_add(1);
    }
    id
}

/// First automatic objective id (a host passes its own entity ids).
const AUTO_ID: u32 = 1_000_000;

/// `rules.match_objective`: place one `flag` (at `position`, or at the symmetric base `offset` away),
/// install the `hill`, `place_flags` for several teams, install a capture `zone`, or set the
/// `zone_rules` (domination, hardpoint, attack/defend; ADR-8649).
pub fn match_objective(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    check_known_and_types("rules.match_objective", params, &[])?;
    let given = ["flag", "hill", "place_flags", "zone", "zone_rules"]
        .iter()
        .filter(|k| params.get(**k).is_some_and(|v| !v.is_null()))
        .count();
    if given != 1 {
        return Err(invalid(
            "give exactly one of `flag`, `hill`, `place_flags`, `zone`, `zone_rules`",
        ));
    }
    let mut rules = session_mut(world)?;
    let session = &mut rules.session;
    let placed: Vec<u32> = if let Some(v) = params.get("flag").filter(|v| !v.is_null()) {
        let flag: FlagParam =
            serde_json::from_value(v.clone()).map_err(|e| invalid(format!("`flag`: {e}")))?;
        let position = match (flag.position, flag.offset) {
            (Some(p), None) => p,
            (None, Some(offset)) => regia_framework::rules::ctf::ctf_base_position(flag.team, offset),
            _ => return Err(invalid("`flag` takes exactly one of `position`, `offset`")),
        };
        let id = flag.id.unwrap_or_else(|| free_id(session, AUTO_ID));
        session
            .board
            .spawn_flag(id, flag.team, position)
            .map_err(|e| refused(&e))?;
        vec![id]
    } else if let Some(v) = params.get("hill").filter(|v| !v.is_null()) {
        let hill: HillParam =
            serde_json::from_value(v.clone()).map_err(|e| invalid(format!("`hill`: {e}")))?;
        let id = hill.id.unwrap_or_else(|| free_id(session, AUTO_ID));
        session
            .board
            .install_hill_volume(
                id,
                HillVolume {
                    min: hill.min,
                    max: hill.max,
                },
            )
            .map_err(|e| refused(&e))?;
        vec![id]
    } else if let Some(v) = params.get("zone").filter(|v| !v.is_null()) {
        let zone: ZoneParam =
            serde_json::from_value(v.clone()).map_err(|e| invalid(format!("`zone`: {e}")))?;
        let id = zone.id.unwrap_or_else(|| free_id(session, AUTO_ID));
        session
            .board
            .install_zone(id, zone.settings())
            .map_err(|e| refused(&e))?;
        vec![id]
    } else if let Some(v) = params.get("zone_rules").filter(|v| !v.is_null()) {
        let rules: ZoneRulesParam =
            serde_json::from_value(v.clone()).map_err(|e| invalid(format!("`zone_rules`: {e}")))?;
        let (mode, score_limit) = rules.mode()?;
        session
            .board
            .set_zone_rules(mode, score_limit)
            .map_err(|e| refused(&e))?;
        Vec::new()
    } else {
        let v = params.get("place_flags").cloned().unwrap_or(Value::Null);
        let place: PlaceFlagsParam =
            serde_json::from_value(v).map_err(|e| invalid(format!("`place_flags`: {e}")))?;
        let before: Vec<u32> = session.board.objectives.iter().map(|o| o.entity_id).collect();
        let first = place.first_id.unwrap_or_else(|| free_id(session, AUTO_ID));
        session
            .board
            .place_flags(&place.teams, place.offset, first)
            .map_err(|e| refused(&e))?;
        session
            .board
            .objectives
            .iter()
            .map(|o| o.entity_id)
            .filter(|id| !before.contains(id))
            .collect()
    };
    let mut out = session_json(session)?;
    if let Value::Object(map) = &mut out {
        map.insert("placed".into(), json!(placed));
    }
    Ok(out)
}

fn yes() -> bool {
    true
}

/// A test hint for where a unit's dropped flag lands: `return` stands for a kill volume, out of
/// bounds or a sealed tunnel at the unit's position.
#[derive(Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum DropHint {
    Keep,
    Return,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UnitParam {
    id: u32,
    team: u8,
    position: [f32; 3],
    #[serde(default = "yes")]
    alive: bool,
    #[serde(default)]
    drop: Option<DropHint>,
}

/// `rules.match_step`: run `steps` lockstep steps (default 1) with the same `units` each step, in
/// the given order. Answers the last step, the steps the next match started on, and every event.
///
/// One `return_zones` box of `rules.match_step` (KI-GP-030): inclusive world bounds.
#[derive(Deserialize, Clone, Copy)]
#[serde(deny_unknown_fields)]
struct ReturnZone {
    min: [f32; 3],
    max: [f32; 3],
}

impl ReturnZone {
    fn contains(&self, p: [f32; 3]) -> bool {
        (0..3).all(|i| self.min[i] <= p[i] && p[i] <= self.max[i])
    }
}

/// Most `return_zones` boxes in one `rules.match_step` call.
pub const MAX_RETURN_ZONES: usize = 64;

/// A unit's `drop: "return"` makes the step's drop check answer [`DropCheck::Return`] for a flag
/// lying exactly at that unit's position (where its carrier died), and `return_zones` for a flag
/// lying inside any of its boxes, wherever it landed (KI-GP-030), standing in for a host's kill
/// volume; every other point is kept.
pub fn match_step(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    check_known_and_types("rules.match_step", params, &[])?;
    let steps = crate::params::opt_u64(params, "steps")?.unwrap_or(1);
    if !(1..=MAX_STEPS).contains(&steps) {
        return Err(invalid(format!("`steps` must be 1..={MAX_STEPS}")));
    }
    let units: Vec<UnitParam> = match params.get("units").filter(|v| !v.is_null()) {
        Some(v) => serde_json::from_value(v.clone()).map_err(|e| invalid(format!("`units`: {e}")))?,
        None => Vec::new(),
    };
    if units.len() > MAX_UNITS {
        return Err(invalid(format!("at most {MAX_UNITS} units")));
    }
    let zones: Vec<ReturnZone> = match params.get("return_zones").filter(|v| !v.is_null()) {
        Some(v) => serde_json::from_value(v.clone()).map_err(|e| invalid(format!("`return_zones`: {e}")))?,
        None => Vec::new(),
    };
    if zones.len() > MAX_RETURN_ZONES {
        return Err(invalid(format!("at most {MAX_RETURN_ZONES} return zones")));
    }
    if let Some(k) = zones
        .iter()
        .position(|z| (0..3).any(|i| !z.min[i].is_finite() || !z.max[i].is_finite() || z.min[i] > z.max[i]))
    {
        return Err(invalid(format!(
            "`return_zones[{k}]`: min and max must be finite with min <= max on each axis"
        )));
    }
    let return_points: Vec<[u32; 3]> = units
        .iter()
        .filter(|u| u.drop == Some(DropHint::Return))
        .map(|u| u.position.map(f32::to_bits))
        .collect();
    let units: Vec<Contestant> = units
        .into_iter()
        .map(|u| Contestant {
            id: u.id,
            team: u.team,
            position: u.position,
            alive: u.alive,
        })
        .collect();
    let mut rules = session_mut(world)?;
    let session = &mut rules.session;
    let mut events = session.drain_events();
    let mut launched = Vec::new();
    let mut last = None;
    for _ in 0..steps {
        let report = session.step_with(&units, |pos| {
            if return_points.contains(&pos.map(f32::to_bits)) || zones.iter().any(|z| z.contains(pos)) {
                DropCheck::Return
            } else {
                DropCheck::Keep
            }
        });
        if report.flow.launched {
            launched.push(report.flow.step);
        }
        events.extend(report.events);
        last = Some(report.flow);
    }
    let mut out = session_json(session)?;
    if let Value::Object(map) = &mut out {
        map.insert("last_step".into(), json!(last));
        map.insert("launched_on".into(), json!(launched));
        map.insert("events".into(), json!(events));
    }
    // A launch rebalance moves seats: retag `TeamId`/`LedgerSlot` in the same call (ADR-8657).
    regia_framework::rules::retag_moved_seats(world);
    Ok(out)
}

fn u32_param(params: &Value, key: &str) -> Result<Option<u32>, RpcError> {
    match crate::params::opt_u64(params, key)? {
        None => Ok(None),
        Some(value) => u32::try_from(value)
            .map(Some)
            .map_err(|_| invalid(format!("`{key}` is too large"))),
    }
}

fn series_json(session: &MatchSession, recorded: Option<bool>) -> Value {
    let series = &session.series;
    json!({
        "length": series.length,
        "played": series.played,
        "wins": series.wins,
        "carry": series.carry,
        "winner": series.winner,
        "max_teams": series.max_teams,
        "need": series.need(),
        "over": series.is_over(),
        "label": series.label(),
        "winner_label": series.winner_label(),
        "row_text": series.row_text(),
        "lives": session.win_rule.lives,
        "rule": session.win_rule.rule().as_str(),
        "recorded": recorded,
    })
}

fn carry_array(value: &Value) -> Result<[u32; SERIES_TEAM_SLOTS], RpcError> {
    let Some(items) = value.as_array() else {
        return Err(invalid("`carry` must be an array of 5 integers"));
    };
    if items.len() != SERIES_TEAM_SLOTS {
        return Err(invalid("`carry` must hold 5 integers"));
    }
    let mut carry = [0; SERIES_TEAM_SLOTS];
    for (index, item) in items.iter().enumerate() {
        let number = item.as_u64().ok_or_else(|| invalid("`carry` must be integers"))?;
        carry[index] = u32::try_from(number).map_err(|_| invalid("`carry` value is too large"))?;
    }
    Ok(carry)
}

/// `rules.series`: read the series, or, as the host, set its length, reset it, record a round,
/// or set the win rule's lives and stock rule. A guest who sends any of those is refused before
/// the series changes.
pub fn series(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    check_known_and_types("rules.series", params, &[])?;
    let issuer = issuer_of(params)?;
    let length = u32_param(params, "length")?;
    let reset = crate::params::opt_bool(params, "reset")?.unwrap_or(false);
    let record = params.get("record").filter(|value| !value.is_null());
    let lives = u32_param(params, "lives")?;
    let rule = crate::params::opt_str(params, "rule")?;
    let mutating = length.is_some() || reset || record.is_some() || lives.is_some() || rule.is_some();
    let mut rules = session_mut(world)?;
    if mutating && !issuer.host {
        return Err(refused(&RulesError::NotPermitted));
    }
    // Reject a bad sibling before any field is written, so a valid length does not stick when
    // lives or the record object is refused.
    let length = match length {
        Some(length) => Some(series_mod::clamp_series(length).map_err(|error| refused(&error))?),
        None => None,
    };
    let lives = match lives {
        Some(lives) => Some(wins::clamp_lives(lives).map_err(|error| refused(&error))?),
        None => None,
    };
    let rule = match rule {
        Some(rule) => {
            Some(StockRule::from_lobby_str(rule).ok_or_else(|| invalid("`rule` is lives or structures"))?)
        }
        None => None,
    };
    let round = match record {
        Some(value) => {
            let winner = value
                .get("winner")
                .and_then(Value::as_u64)
                .ok_or_else(|| invalid("`record.winner` is required"))?;
            let winner = u8::try_from(winner).map_err(|_| invalid("`record.winner` is too large"))?;
            let carry = match value.get("carry") {
                Some(carry) => carry_array(carry)?,
                None => [0; SERIES_TEAM_SLOTS],
            };
            Some(RoundResult { winner, carry })
        }
        None => None,
    };
    let session = &mut rules.session;
    if let Some(length) = length {
        let max_teams = session.series.max_teams;
        session.series = SeriesState::new(length);
        session.series.max_teams = max_teams;
    }
    if reset {
        session.series.reset();
    }
    let mut recorded = None;
    if let Some(round) = round.as_ref() {
        recorded = Some(
            session
                .record_round_from(issuer, round)
                .map_err(|error| refused(&error))?,
        );
    }
    if let Some(lives) = lives {
        session.win_rule.lives = lives;
    }
    if let Some(rule) = rule {
        session.win_rule.set_rule(rule);
    }
    Ok(series_json(session, recorded))
}

fn vote_json(session: &MatchSession, now_tick: u64) -> Value {
    let Some(vote) = session.vote.as_ref() else {
        return json!({
            "open": false,
            "closed": true,
            "options": [],
            "tally": [],
            "winner": Value::Null,
            "winner_map": Value::Null,
            "opened_tick": Value::Null,
            "close_tick": Value::Null,
            "vote_count": 0,
        });
    };
    json!({
        "open": vote.is_open(now_tick),
        "closed": vote.expired(now_tick),
        "options": vote.options,
        "tally": vote.tally(),
        "winner": vote.winner(),
        "winner_map": vote.winner_map(),
        "opened_tick": vote.opened_tick,
        "close_tick": vote.close_tick,
        "vote_count": vote.vote_count(),
    })
}

/// `rules.vote_cast`: one ballot. `voter` and `option` are required. A guest may cast only the
/// voter id of their own seat, and that check runs before the window.
pub fn vote_cast(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    check_known_and_types("rules.vote_cast", params, &[])?;
    let issuer = issuer_of(params)?;
    let voter = u32_param(params, "voter")?.ok_or_else(|| invalid("`voter` is required"))?;
    let option = crate::params::opt_u64(params, "option")?.ok_or_else(|| invalid("`option` is required"))?;
    let option = usize::try_from(option).map_err(|_| invalid("`option` is too large"))?;
    let now_tick = crate::params::opt_u64(params, "now_tick")?.unwrap_or(0);
    let mut rules = session_mut(world)?;
    rules
        .session
        .cast_vote_from(issuer, voter, option, now_tick)
        .map_err(|error| refused(&error))?;
    Ok(vote_json(&rules.session, now_tick))
}

/// `rules.vote_status`: the open ballot, or a closed object when none is open.
pub fn vote_status(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    check_known_and_types("rules.vote_status", params, &[])?;
    let now_tick = crate::params::opt_u64(params, "now_tick")?.unwrap_or(0);
    let rules = world.get_resource::<MatchRules>().ok_or_else(unavailable)?;
    Ok(vote_json(&rules.session, now_tick))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KillNote {
    tick: u64,
    killer_team: u8,
    victim_team: u8,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StructureNote {
    tick: u64,
    team: u8,
    #[serde(default)]
    stem: String,
    #[serde(default)]
    producer: bool,
    #[serde(default)]
    primary: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlayerRow {
    team: u8,
    name: String,
    #[serde(default)]
    kills: u32,
    #[serde(default)]
    deaths: u32,
    #[serde(default)]
    ships_lost: u32,
    #[serde(default)]
    gathered: u32,
    #[serde(default)]
    spent: u32,
    #[serde(default)]
    units_made: u32,
    #[serde(default)]
    buildings_made: u32,
    #[serde(default)]
    is_ai: bool,
}

fn end_stats_json(session: &MatchSession, players: Option<&[PlayerEndStats]>) -> Value {
    let ledger = &session.ledger;
    let length = ledger::format_match_length(ledger.length_ticks(ledger.start_tick), ledger.tick_hz);
    let (mvp_index, mvp, teams) = match players {
        Some(players) => {
            let index = ledger::mvp_index(players);
            let line = index.map(|at| ledger::mvp_line(&players[at]));
            let teams: Vec<Value> = ledger::aggregate_players(players)
                .into_iter()
                .map(|team| {
                    json!({
                        "team": team.team,
                        "players": team.players,
                        "kills": team.kills,
                        "deaths": team.deaths,
                        "ships_lost": team.ships_lost,
                        "gathered": team.gathered,
                        "spent": team.spent,
                        "units_made": team.units_made,
                        "buildings_made": team.buildings_made,
                    })
                })
                .collect();
            (index, line, teams)
        }
        None => (None, None, Vec::new()),
    };
    json!({
        "start_tick": ledger.start_tick,
        "over_tick": ledger.over_tick,
        "tick_hz": ledger.tick_hz,
        "lines": ledger.timeline_lines(),
        "length": length,
        "mvp_index": mvp_index,
        "mvp": mvp,
        "teams": teams,
    })
}

/// `rules.end_stats`: the ledger. Optional `note_kill` and `note_structure` are host-only.
/// Optional `players` adds the team totals and the MVP.
pub fn end_stats(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    check_known_and_types("rules.end_stats", params, &[])?;
    let issuer = issuer_of(params)?;
    let kill = match params.get("note_kill").filter(|value| !value.is_null()) {
        Some(value) => Some(
            serde_json::from_value::<KillNote>(value.clone())
                .map_err(|error| invalid(format!("`note_kill`: {error}")))?,
        ),
        None => None,
    };
    let structure = match params.get("note_structure").filter(|value| !value.is_null()) {
        Some(value) => Some(
            serde_json::from_value::<StructureNote>(value.clone())
                .map_err(|error| invalid(format!("`note_structure`: {error}")))?,
        ),
        None => None,
    };
    let players = match params.get("players").filter(|value| !value.is_null()) {
        Some(value) => {
            let rows: Vec<PlayerRow> = serde_json::from_value(value.clone())
                .map_err(|error| invalid(format!("`players`: {error}")))?;
            Some(
                rows.into_iter()
                    .map(|row| PlayerEndStats {
                        team: row.team,
                        name: row.name,
                        kills: row.kills,
                        deaths: row.deaths,
                        ships_lost: row.ships_lost,
                        gathered: row.gathered,
                        spent: row.spent,
                        units_made: row.units_made,
                        buildings_made: row.buildings_made,
                        is_ai: row.is_ai,
                    })
                    .collect::<Vec<_>>(),
            )
        }
        None => None,
    };
    let mut rules = session_mut(world)?;
    if (kill.is_some() || structure.is_some()) && !issuer.host {
        return Err(refused(&RulesError::NotPermitted));
    }
    let session = &mut rules.session;
    if let Some(note) = &kill {
        session.note_kill(note.tick, note.killer_team, note.victim_team);
    }
    if let Some(note) = &structure {
        session.note_structure(note.tick, note.team, &note.stem, note.producer, note.primary);
    }
    Ok(end_stats_json(session, players.as_deref()))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AssignSeat {
    #[serde(default)]
    team: Option<u8>,
    #[serde(default)]
    slot: Option<u8>,
    #[serde(default)]
    unit: Option<u32>,
    #[serde(default)]
    from: Option<AsSeat>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CounterParam {
    team: u8,
    slot: u8,
    name: String,
    delta: i64,
}

fn assign_request(seat: &AssignSeat) -> SeatRequest {
    SeatRequest {
        from: seat.from.as_ref().map(|from| SeatId::new(from.team, from.slot)),
        team: seat.team,
        slot: seat.slot,
        unit: seat.unit,
    }
}

fn opt_u8(params: &Value, key: &str) -> Result<Option<u8>, RpcError> {
    match crate::params::opt_u64(params, key)? {
        None => Ok(None),
        Some(value) => u8::try_from(value)
            .map(Some)
            .map_err(|_| invalid(format!("`{key}` must be 0..=255"))),
    }
}

fn opt_u32(params: &Value, key: &str) -> Result<Option<u32>, RpcError> {
    match crate::params::opt_u64(params, key)? {
        None => Ok(None),
        Some(value) => u32::try_from(value)
            .map(Some)
            .map_err(|_| invalid(format!("`{key}` must be 0..=4294967295"))),
    }
}

fn seat_json(seats: &[SeatId]) -> Value {
    json!(
        seats
            .iter()
            .map(|seat| json!({ "team": seat.team, "slot": seat.slot }))
            .collect::<Vec<_>>()
    )
}

/// `rules.match_assign`: seat one player, a party, or add a named counter.
///
/// `{}` seats one player on the smallest team. `group` is the party (the first object is the
/// request, the rest follow it) and excludes the top-level seat fields. `counter` is
/// `{ team, slot, name, delta }` and excludes both. A team change the switch policy refuses
/// answers `match_team_switch_refused`. A seventeenth new counter name answers `match_stat_cap`.
pub fn match_assign(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    check_known_and_types("rules.match_assign", params, &[])?;
    let has_group = params.get("group").is_some_and(|value| !value.is_null());
    let has_counter = params.get("counter").is_some_and(|value| !value.is_null());
    let has_fields = ["team", "slot", "unit", "from"]
        .iter()
        .any(|key| params.get(*key).is_some_and(|value| !value.is_null()));
    if (has_group && has_counter) || (has_group && has_fields) || (has_counter && has_fields) {
        return Err(invalid("group, counter and seat fields exclude each other"));
    }
    let mut rules = session_mut(world)?;
    if has_counter {
        let counter: CounterParam = serde_json::from_value(params["counter"].clone())
            .map_err(|error| invalid(format!("`counter`: {error}")))?;
        rules
            .session
            .add_counter(
                SeatId::new(counter.team, counter.slot),
                &counter.name,
                counter.delta,
            )
            .map_err(|error| refused(&error))?;
        return session_json(&rules.session);
    }
    let (request, group) = if has_group {
        let seats: Vec<AssignSeat> = serde_json::from_value(params["group"].clone())
            .map_err(|error| invalid(format!("`group`: {error}")))?;
        if seats.is_empty() {
            return Err(invalid("`group` needs at least one seat"));
        }
        if seats.len() > MAX_ASSIGN_GROUP {
            return Err(invalid(format!("`group` holds at most {MAX_ASSIGN_GROUP} seats")));
        }
        let mut seats = seats;
        let rest = seats.split_off(1);
        let Some(first) = seats.pop() else {
            return Err(invalid("`group` needs at least one seat"));
        };
        let group: Vec<SeatRequest> = rest.iter().map(assign_request).collect();
        (assign_request(&first), group)
    } else {
        let from = match params.get("from").filter(|value| !value.is_null()) {
            Some(value) => Some(
                serde_json::from_value::<AsSeat>(value.clone())
                    .map_err(|error| invalid(format!("`from`: {error}")))?,
            ),
            None => None,
        };
        let request = SeatRequest {
            from: from.map(|from| SeatId::new(from.team, from.slot)),
            team: opt_u8(params, "team")?,
            slot: opt_u8(params, "slot")?,
            unit: opt_u32(params, "unit")?,
        };
        (request, Vec::new())
    };
    let seats = rules
        .session
        .assign_team(&request, &group)
        .map_err(|error| refused(&error))?;
    let mut out = session_json(&rules.session)?;
    if let Value::Object(map) = &mut out {
        map.insert("seats".into(), seat_json(&seats));
    }
    // Retag the moved entities in the same call, so damage credits the moved rows (ADR-8657).
    regia_framework::rules::retag_moved_seats(world);
    Ok(out)
}

/// `rules.spawn_preview`: every `PlayerStart`'s eligibility and score terms for `player`, the start
/// a pick would choose now (the session's spawn state does not move), the respawn settings and the
/// running match's respawn book (ADR-8647). Works without a [`MatchRules`] resource (policy
/// `first`). An entity that does not exist answers `spawn_player_missing`.
pub fn spawn_preview(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    check_known_and_types("rules.spawn_preview", params, &[])?;
    let bits = params
        .get("player")
        .and_then(Value::as_u64)
        .ok_or_else(|| invalid("`player` (an entity id) is required"))?;
    let player = Entity::try_from_bits(bits)
        .filter(|e| world.get_entity(*e).is_ok())
        .ok_or_else(|| {
            RpcError::new(
                protocol::INVALID_PARAMS,
                "spawn_player_missing",
                format!("entity {bits} does not exist"),
                "pass the player (or its pawn) entity id from scene.query_entities",
            )
        })?;
    let preview = regia_framework::spawn_select::spawn_preview(world, player);
    let points: Vec<Value> = preview
        .rows
        .iter()
        .map(|row| {
            json!({
                "entity": row.entity.to_bits(),
                "position": row.position.to_array(),
                "team": row.point.team,
                "tags": row.point.tags,
                "priority": row.point.priority,
                "enabled": row.point.enabled,
                "eligible": row.terms.refused.is_none(),
                "refused": row.terms.refused.map(|r| r.word()),
                "nearest_enemy_cm": row.terms.nearest_enemy_cm,
                "enemies_in_sight": row.terms.enemies_in_sight,
                "teammates_near": row.terms.teammates_near,
                "recent_uses": row.terms.recent_uses,
                "score": row.terms.score,
            })
        })
        .collect();
    let (respawn, respawns) = match world.get_resource::<MatchRules>() {
        Some(rules) => (
            serde_json::to_value(&rules.session.respawn),
            serde_json::to_value(&rules.session.respawns),
        ),
        None => (
            serde_json::to_value(regia_framework::rules::spawn::RespawnRules::default()),
            serde_json::to_value(regia_framework::rules::spawn::RespawnBook::default()),
        ),
    };
    let respawn = respawn.map_err(|e| RpcError::internal(format!("serializing respawn: {e}")))?;
    let respawns = respawns.map_err(|e| RpcError::internal(format!("serializing respawns: {e}")))?;
    Ok(json!({
        "policy": preview.policy.word(),
        "order": if preview.arrival_order { "arrival" } else { "position" },
        "team": preview.ask.team,
        "side": preview.ask.side,
        "tick": preview.ask.tick,
        "pick": preview.pick.map(Entity::to_bits),
        "points": points,
        "respawn": respawn,
        "respawns": respawns,
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaggedParam {
    team: u8,
    tag: String,
    count: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HolderParam {
    objective: u32,
    team: u8,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ScoreParam {
    team: u8,
    score: i64,
}

/// Most rows of one `rules.match_report` table.
pub const MAX_REPORT_ROWS: usize = 256;

fn report_rows<T: serde::de::DeserializeOwned>(params: &Value, key: &str) -> Result<Vec<T>, RpcError> {
    let Some(value) = params.get(key).filter(|v| !v.is_null()) else {
        return Ok(Vec::new());
    };
    let rows: Vec<T> = serde_json::from_value(value.clone()).map_err(|e| invalid(format!("`{key}`: {e}")))?;
    if rows.len() > MAX_REPORT_ROWS {
        return Err(invalid(format!("`{key}` holds at most {MAX_REPORT_ROWS} rows")));
    }
    Ok(rows)
}

/// `rules.match_report`: the host's reports for the strategy victory conditions (GR-11.4):
/// `tagged` counts (`destroy_all_tagged`), objective `holders` (`hold_for`), team `scores`
/// (`score_threshold`) and `defeat` (teams the host puts out). Every table is checked before
/// any row is applied; a refused tag answers `match_victory_settings`. The answer is
/// rules.match_get's.
pub fn match_report(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    check_known_and_types("rules.match_report", params, &[])?;
    let tagged: Vec<TaggedParam> = report_rows(params, "tagged")?;
    let holders: Vec<HolderParam> = report_rows(params, "holders")?;
    let scores: Vec<ScoreParam> = report_rows(params, "scores")?;
    let defeat: Vec<u8> = report_rows(params, "defeat")?;
    if tagged
        .iter()
        .any(|row| row.tag.is_empty() || row.tag.len() > regia_framework::rules::victory::MAX_TAG_LEN)
    {
        return Err(refused(&RulesError::VictorySettings { field: "tag" }));
    }
    {
        let mut rules = session_mut(world)?;
        let session = &mut rules.session;
        // Every table is checked, the session's row cap included, before any row lands
        // (rules-genre-br-rts-review finding C): an update ahead of a refused insert must not stay.
        let tag_keys: Vec<(u8, &str)> = tagged.iter().map(|row| (row.team, row.tag.as_str())).collect();
        let hold_keys: Vec<u32> = holders.iter().map(|row| row.objective).collect();
        let score_keys: Vec<u8> = scores.iter().map(|row| row.team).collect();
        session
            .victory_state
            .check_report(&tag_keys, &hold_keys, &score_keys)
            .map_err(|e| refused(&e))?;
        for row in &tagged {
            session
                .report_tagged(row.team, &row.tag, row.count)
                .map_err(|e| refused(&e))?;
        }
        for row in &holders {
            session
                .report_holder(row.objective, row.team)
                .map_err(|e| refused(&e))?;
        }
        for row in &scores {
            session
                .report_score(row.team, row.score)
                .map_err(|e| refused(&e))?;
        }
        for team in &defeat {
            session.defeat_team(*team);
        }
    }
    match_get(world, &Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(error: &RpcError) -> Option<&str> {
        error.data.as_ref()?.get("code")?.as_str()
    }

    #[test]
    fn every_method_but_setup_says_unavailable_without_the_resource() {
        let mut bare = World::new();
        let empty = json!({});
        for result in [
            match_get(&mut bare, &empty),
            match_input(&mut bare, &json!({ "input": { "input": "open_between" } })),
            match_objective(
                &mut bare,
                &json!({ "hill": { "min": [0, 0, 0], "max": [1, 1, 1] } }),
            ),
            match_step(&mut bare, &empty),
            series(&mut bare, &empty),
            vote_cast(&mut bare, &json!({ "voter": 1, "option": 0 })),
            vote_status(&mut bare, &empty),
            end_stats(&mut bare, &empty),
        ] {
            assert_eq!(
                code(&result.expect_err("unavailable")),
                Some("match_rules_unavailable")
            );
        }
        match_setup(&mut bare, &empty).expect("setup inserts the resource");
        match_get(&mut bare, &empty).expect("now present");
    }

    #[test]
    fn a_ready_up_through_the_methods_starts_on_the_agreed_step() {
        let mut world = World::new();
        let setup = match_setup(
            &mut world,
            &json!({
                "settings": { "flow": { "start_delay_ticks": 5 } },
                "playlist": [{ "map_id": "harbor", "mode": "ctf" }],
                "between": true,
            }),
        )
        .expect("setup");
        assert_eq!(setup["flow"]["between"], true);
        match_input(
            &mut world,
            &json!({ "inputs": [
                { "input": "seat", "team": 1, "slot": 0 },
                { "input": "seat", "team": 2, "slot": 0 },
            ] }),
        )
        .expect("seats");
        let ready = match_input(
            &mut world,
            &json!({ "inputs": [
                { "input": "ready", "team": 1, "slot": 0 },
                { "input": "ready", "team": 2, "slot": 0 },
            ] }),
        )
        .expect("readies");
        assert_eq!(ready["flow"]["starts_at"], 5);
        assert_eq!(ready["ready_counts"], json!({ "ready": 2, "need": 2 }));
        let stepped = match_step(&mut world, &json!({ "steps": 10 })).expect("step");
        assert_eq!(stepped["launched_on"], json!([5]));
        assert_eq!(stepped["mode"], "ctf");
        assert_eq!(stepped["current"]["entry"]["map_id"], "harbor");
        assert!(
            stepped["events"]
                .as_array()
                .expect("events")
                .iter()
                .any(|e| e["kind"] == "launched")
        );
        crate::contract::assert_result_named("rules.match_setup", &setup);
        crate::contract::assert_result_named("rules.match_input", &ready);
        crate::contract::assert_result_named("rules.match_step", &stepped);
        let got = match_get(&mut world, &json!({})).expect("get");
        crate::contract::assert_result_named("rules.match_get", &got);
    }

    #[test]
    fn a_flag_raid_scores_through_the_methods() {
        let mut world = World::new();
        match_setup(
            &mut world,
            &json!({
                "settings": { "objectives": { "capture_limit": 1 } },
                "playlist": [{ "map_id": "harbor", "mode": "ctf" }],
                "start": true,
            }),
        )
        .expect("setup");
        let placed = match_objective(
            &mut world,
            &json!({ "place_flags": { "teams": [1, 2], "offset": 50.0 } }),
        )
        .expect("flags");
        assert_eq!(placed["placed"].as_array().map(Vec::len), Some(2));
        crate::contract::assert_result_named("rules.match_objective", &placed);
        // Team 1's raider takes team 2's flag, then walks it home.
        match_step(
            &mut world,
            &json!({ "units": [{ "id": 7, "team": 1, "position": [50.0, 0.0, 0.0] }] }),
        )
        .expect("take");
        let home = match_step(
            &mut world,
            &json!({ "units": [{ "id": 7, "team": 1, "position": [-50.0, 0.0, 0.0] }] }),
        )
        .expect("capture");
        assert_eq!(home["board"]["team_scores"][1], 1);
        assert_eq!(home["board"]["match_over"], true);
        assert_eq!(home["board"]["end_reason"], "capture_limit_reached");
    }

    #[test]
    fn capture_zones_take_progress_and_flip_through_the_methods() {
        let mut world = World::new();
        match_setup(
            &mut world,
            &json!({ "playlist": [{ "map_id": "depot", "mode": "dom" }], "start": true }),
        )
        .expect("setup");
        for (id, x) in [(1, 0.0), (2, 100.0)] {
            let placed = match_objective(
                &mut world,
                &json!({ "zone": {
                    "id": id, "min": [x - 5.0, -1.0, -5.0], "max": [x + 5.0, 9.0, 5.0],
                    "capture_ticks": 3, "contest": "majority", "owner_scores": false
                } }),
            )
            .expect("zone");
            assert_eq!(placed["placed"], json!([id]));
            crate::contract::assert_result_named("rules.match_objective", &placed);
        }
        let unit = json!({ "units": [{ "id": 7, "team": 1, "position": [0.0, 0.0, 0.0] }], "steps": 2 });
        let halfway = match_step(&mut world, &unit).expect("step");
        let zone = &halfway["board"]["objectives"][0];
        assert_eq!(zone["kind"], "zone");
        assert_eq!(zone["zone"]["capturing_team"], 1);
        assert_eq!(zone["zone"]["progress"], 2);
        assert_eq!(zone["zone"]["settings"]["contest"], "majority");
        let flipped = match_step(&mut world, &json!({ "units": unit["units"] })).expect("step");
        assert_eq!(flipped["board"]["objectives"][0]["zone"]["owner"], 1);
        assert!(
            flipped["events"]
                .as_array()
                .is_some_and(|e| e.iter().any(|e| e["kind"] == "zone_captured" && e["zone"] == 1)),
            "{}",
            flipped["events"]
        );
        let got = match_get(&mut world, &json!({})).expect("get");
        assert_eq!(got["board"]["objectives"][0]["zone"]["owner"], 1);
        // Zone rules: a hardpoint over both zones, then the refusals.
        let rules = match_objective(
            &mut world,
            &json!({ "zone_rules": { "mode": "hardpoint", "rotation": [2, 1], "rotate_ticks": 30 } }),
        )
        .expect("hardpoint");
        assert_eq!(rules["placed"], json!([]));
        assert_eq!(rules["board"]["zone_rules"]["mode"]["kind"], "hardpoint");
        assert_eq!(rules["board"]["objectives"][0]["zone"]["locked"], true);
        let err = match_objective(
            &mut world,
            &json!({ "zone_rules": { "mode": "hardpoint", "rotation": [9], "rotate_ticks": 30 } }),
        )
        .expect_err("unknown");
        assert_eq!(code(&err), Some("match_zone_unknown"));
        let err = match_objective(
            &mut world,
            &json!({ "zone_rules": { "mode": "hardpoint", "rotation": [1], "rotate_ticks": 0 } }),
        )
        .expect_err("zero");
        assert_eq!(code(&err), Some("match_zone_settings"));
        let err = match_objective(
            &mut world,
            &json!({ "zone_rules": { "mode": "attack_defend", "attackers": 1, "defenders": 1, "order": [1] } }),
        )
        .expect_err("same team");
        assert_eq!(code(&err), Some("match_zone_settings"));
        // Back to domination: every zone unlocks again (both were installed unlocked).
        let back =
            match_objective(&mut world, &json!({ "zone_rules": { "mode": "domination" } })).expect("dom");
        assert_eq!(back["board"]["objectives"][0]["zone"]["locked"], false);
        for bad in [
            json!({ "zone_rules": { "mode": "tug" } }),
            json!({ "zone_rules": { "mode": "hardpoint", "rotation": [1] } }),
            json!({ "zone": { "min": [0.0, 0.0, 0.0] } }),
            json!({ "zone": { "min": [0.0, 0.0, 0.0], "max": [1.0, 1.0, 1.0], "contest": "loudest" } }),
        ] {
            let err = match_objective(&mut world, &bad).expect_err("bad");
            assert_eq!(code(&err), Some("F0002"), "{bad}");
        }
    }

    #[test]
    fn refusals_carry_the_rules_code_and_bad_params_f0002() {
        let mut world = World::new();
        match_setup(&mut world, &json!({})).expect("setup");
        let err = match_input(
            &mut world,
            &json!({ "input": { "input": "ready", "team": 1, "slot": 0 } }),
        )
        .expect_err("live");
        assert_eq!(code(&err), Some("match_live"));
        assert_eq!(
            err.data.as_ref().map(|d| d["reason"].clone()),
            Some(json!("match_live"))
        );
        let err = match_input(
            &mut world,
            &json!({ "input": { "input": "resume", "name": "x" } }),
        )
        .expect_err("not paused");
        assert_eq!(code(&err), Some("match_not_paused"));
        let err =
            match_objective(&mut world, &json!({ "flag": { "team": 9, "offset": 10.0 } })).expect_err("team");
        assert_eq!(code(&err), Some("match_flag_team"));
        // Every rules code is listed by the method that can answer it.
        for rules_code in RulesError::CODES {
            let method = if rules_code == "match_team_switch_refused" || rules_code == "match_stat_cap" {
                "rules.match_assign"
            } else if rules_code.starts_with("match_flag")
                || rules_code.starts_with("match_hill")
                || rules_code.starts_with("match_objective")
                || rules_code.starts_with("match_zone")
            {
                "rules.match_objective"
            } else if rules_code == "match_phase_settings"
                || rules_code == "match_wave_settings"
                || rules_code == "match_race_settings"
                || rules_code == "match_shrink_zone_settings"
                || rules_code == "match_victory_settings"
            {
                "rules.match_setup"
            } else if rules_code == "vote_closed" || rules_code == "vote_bad_option" {
                "rules.vote_cast"
            } else if rules_code == "series_length" || rules_code == "lives_range" {
                "rules.series"
            } else {
                "rules.match_input"
            };
            crate::contract::assert_listed(method, rules_code);
        }
        // ADR-8485: phase settings that cannot run are refused; the answer shows the phase fields.
        let err =
            match_setup(&mut world, &json!({ "phases": { "time_limit_ticks": 0 } })).expect_err("phases");
        assert_eq!(code(&err), Some("match_phase_settings"));
        let ok = match_setup(
            &mut world,
            &json!({ "start": true, "phases": { "time_limit_ticks": 600, "rounds": { "rounds_to_win": 2 } } }),
        )
        .expect("setup");
        assert_eq!(ok["phase"], json!("live"));
        assert_eq!(ok["round"], json!(1));
        assert_eq!(ok["time_left_ticks"], json!(600));
        assert_eq!(ok["flow"]["settings"]["phases"]["time_limit_ticks"], json!(600));
        for method in [
            "rules.match_get",
            "rules.match_input",
            "rules.match_objective",
            "rules.match_step",
            "rules.series",
            "rules.vote_cast",
            "rules.vote_status",
            "rules.end_stats",
            "rules.match_assign",
        ] {
            crate::contract::assert_listed(method, "match_rules_unavailable");
        }
        let err = match_input(&mut world, &json!({ "input": { "input": "dance" } })).expect_err("bad");
        assert_eq!(code(&err), Some("F0002"));
        let err = match_step(&mut world, &json!({ "steps": 0 })).expect_err("bad");
        assert_eq!(code(&err), Some("F0002"));
        let err = match_objective(&mut world, &json!({})).expect_err("none");
        assert_eq!(code(&err), Some("F0002"));
        let err = match_setup(&mut world, &json!({ "start": true, "between": true })).expect_err("both");
        assert_eq!(code(&err), Some("F0002"));
    }

    #[test]
    fn match_input_as_a_guest_is_refused_with_match_not_permitted() {
        let mut world = World::new();
        match_setup(
            &mut world,
            &json!({
                "settings": { "flow": { "start_delay_ticks": 0 } },
                "between": true,
            }),
        )
        .expect("setup");
        match_input(
            &mut world,
            &json!({ "inputs": [
                { "input": "seat", "team": 1, "slot": 0 },
                { "input": "seat", "team": 2, "slot": 0 },
            ] }),
        )
        .expect("host seats");
        let err = match_input(
            &mut world,
            &json!({
                "as": { "team": 1, "slot": 0 },
                "input": { "input": "ready", "team": 2, "slot": 0 },
            }),
        )
        .expect_err("another seat");
        assert_eq!(code(&err), Some("match_not_permitted"));
        let got = match_get(&mut world, &json!({})).expect("get");
        assert!(got["flow"].get("joins").is_some());
        assert!(got["flow"].get("paused_by_seat").is_some());
        assert!(got["flow"].get("join").is_none());
        assert_eq!(got["flow"]["ready"], json!([]));
        match_input(
            &mut world,
            &json!({
                "as": { "team": 1, "slot": 0 },
                "input": { "input": "ready", "team": 1, "slot": 0 },
            }),
        )
        .expect("own seat");
        let err = match_input(
            &mut world,
            &json!({
                "as": { "team": 1, "slot": 0 },
                "input": { "input": "host_pause", "on": true },
            }),
        )
        .expect_err("host pause");
        assert_eq!(code(&err), Some("match_not_permitted"));
    }

    #[test]
    fn multi_level_settings_and_a_drop_hint_work_through_the_methods() {
        let mut world = World::new();
        let setup = match_setup(
            &mut world,
            &json!({
                "settings": { "objectives": {
                    "touch_height": 2.0,
                    "flag_return_ticks": 30,
                    "capture_needs_home_flag": false,
                } },
                "playlist": [{ "map_id": "tunnels", "mode": "ctf" }],
                "start": true,
            }),
        )
        .expect("setup");
        let objectives = &setup["board"]["settings"];
        assert_eq!(objectives["touch_height"], 2.0);
        assert_eq!(objectives["flag_return_ticks"], 30);
        assert_eq!(objectives["capture_needs_home_flag"], false);
        match_objective(
            &mut world,
            &json!({ "flag": { "team": 2, "id": 2, "position": [50.0, 10.0, 0.0] } }),
        )
        .expect("flag");
        // From the tunnel under the base: no pickup.
        let below = match_step(
            &mut world,
            &json!({ "units": [{ "id": 7, "team": 1, "position": [50.0, 0.0, 0.0] }] }),
        )
        .expect("below");
        assert_eq!(below["board"]["objectives"][0]["at_base"], true);
        // On the base's floor: pickup. Then the carrier dies over a kill volume.
        match_step(
            &mut world,
            &json!({ "units": [{ "id": 7, "team": 1, "position": [50.0, 10.0, 0.0] }] }),
        )
        .expect("take");
        let fell = match_step(
            &mut world,
            &json!({ "units": [{
                "id": 7, "team": 1, "position": [20.0, -40.0, 0.0], "alive": false, "drop": "return",
            }] }),
        )
        .expect("fall");
        assert_eq!(fell["board"]["objectives"][0]["at_base"], true);
        let events = fell["events"].as_array().expect("events");
        assert!(
            events
                .iter()
                .any(|e| e["kind"] == "flag_auto_returned" && e["cause"] == "drop_check"),
            "{events:?}"
        );
        crate::contract::assert_result_named("rules.match_step", &fell);
        let err = match_step(
            &mut world,
            &json!({ "units": [{ "id": 7, "team": 1, "position": [0.0, 0.0, 0.0], "drop": "lava" }] }),
        )
        .expect_err("bad hint");
        assert_eq!(code(&err), Some("F0002"));
        let err = match_setup(
            &mut world,
            &json!({ "settings": { "objectives": { "flag_return_ticks": -1 } } }),
        )
        .expect_err("negative ticks");
        assert_eq!(code(&err), Some("F0002"));
    }

    /// KI-GP-030 (ADR-10070): `return_zones` sends home a flag that lies inside a box, wherever it
    /// landed (the `drop` hint covers only the dead carrier's own position); a flag outside every box
    /// stays; a bad box is refused.
    #[test]
    fn return_zones_send_home_a_flag_lying_inside_them() {
        let mut world = World::new();
        match_setup(
            &mut world,
            &json!({ "playlist": [{ "map_id": "tunnels", "mode": "ctf" }], "start": true }),
        )
        .expect("setup");
        match_objective(
            &mut world,
            &json!({ "flag": { "team": 2, "id": 2, "position": [50.0, 10.0, 0.0] } }),
        )
        .expect("flag");
        match_step(
            &mut world,
            &json!({ "units": [{ "id": 7, "team": 1, "position": [50.0, 10.0, 0.0] }] }),
        )
        .expect("take");
        // The carrier dies at (20, -40, 0); the zone is elsewhere: the flag stays.
        let zone_elsewhere = json!([{ "min": [100.0, -50.0, -5.0], "max": [110.0, 0.0, 5.0] }]);
        let kept = match_step(
            &mut world,
            &json!({
                "units": [{ "id": 7, "team": 1, "position": [20.0, -40.0, 0.0], "alive": false }],
                "return_zones": zone_elsewhere,
            }),
        )
        .expect("kept");
        assert_eq!(kept["board"]["objectives"][0]["at_base"], false);
        // The same lying flag inside a zone (not at any unit's position): home.
        let pit = json!([{ "min": [10.0, -50.0, -5.0], "max": [30.0, -30.0, 5.0] }]);
        let home = match_step(&mut world, &json!({ "return_zones": pit })).expect("home");
        assert_eq!(home["board"]["objectives"][0]["at_base"], true);
        let events = home["events"].as_array().expect("events");
        assert!(
            events
                .iter()
                .any(|e| e["kind"] == "flag_auto_returned" && e["cause"] == "drop_check"),
            "{events:?}"
        );
        crate::contract::assert_result_named("rules.match_step", &home);
        for bad in [
            json!([{ "min": [1.0, 0.0, 0.0], "max": [0.0, 1.0, 1.0] }]),
            json!([{ "min": [0.0, 0.0], "max": [1.0, 1.0, 1.0] }]),
            json!([{ "min": [0.0, 0.0, 0.0], "max": [1.0, 1.0, 1.0], "size": 2 }]),
        ] {
            let err = match_step(&mut world, &json!({ "return_zones": bad })).expect_err("bad zone");
            assert_eq!(code(&err), Some("F0002"), "{bad}");
        }
        let many: Vec<Value> = (0..=MAX_RETURN_ZONES)
            .map(|_| json!({ "min": [0.0, 0.0, 0.0], "max": [1.0, 1.0, 1.0] }))
            .collect();
        let err = match_step(&mut world, &json!({ "return_zones": many })).expect_err("too many");
        assert_eq!(code(&err), Some("F0002"));
    }

    /// The match methods are discoverable: each is in the schema, named by an `agent.guide` topic
    /// and explained by `errors.explain` for every rules code (the workspace-wide checks stop at
    /// their first miss, so a miss of another feature must not hide one of these).
    #[test]
    fn the_match_methods_are_in_the_schema_the_guide_and_errors_explain() {
        let methods = [
            "rules.match_get",
            "rules.match_setup",
            "rules.match_input",
            "rules.match_objective",
            "rules.match_step",
            "rules.series",
            "rules.vote_cast",
            "rules.vote_status",
            "rules.end_stats",
            "rules.match_assign",
        ];
        for method in methods {
            assert!(
                crate::schema::METHODS.iter().any(|m| m.name == method),
                "{method} is in the schema"
            );
            assert!(
                crate::discovery::guide_mentions(method),
                "{method} is named by a GUIDE_TOPICS entry"
            );
        }
        for rules_code in RulesError::CODES.iter().chain(["match_rules_unavailable"].iter()) {
            assert!(
                crate::discovery::errors_explain(&json!({ "code": rules_code })).is_ok(),
                "errors.explain knows {rules_code}"
            );
        }
    }

    #[test]
    fn series_vote_and_end_stats_follow_the_issuer() {
        let mut world = World::new();
        match_setup(&mut world, &json!({})).expect("setup");
        let read = series(&mut world, &json!({})).expect("read");
        assert_eq!(read["length"], 1);
        assert_eq!(read["row_text"], "SINGLE MATCH");
        crate::contract::assert_result_named("rules.series", &read);
        let err = series(
            &mut world,
            &json!({ "as": { "team": 1, "slot": 0 }, "record": { "winner": 1 } }),
        )
        .expect_err("guest record");
        assert_eq!(code(&err), Some("match_not_permitted"));
        let err = series(&mut world, &json!({ "length": 2 })).expect_err("bad length");
        assert_eq!(code(&err), Some("series_length"));
        let err = series(&mut world, &json!({ "lives": 0 })).expect_err("lives");
        assert_eq!(code(&err), Some("lives_range"));
        let set = series(
            &mut world,
            &json!({ "length": 3, "lives": 3, "rule": "structures" }),
        )
        .expect("host");
        assert_eq!(set["length"], 3);
        assert_eq!(set["rule"], "structures");
        let recorded = series(
            &mut world,
            &json!({ "record": { "winner": 1, "carry": [0, 4, 0, 0, 0] } }),
        )
        .expect("record");
        assert_eq!(recorded["recorded"], true);
        assert_eq!(recorded["played"], 1);
        assert_eq!(recorded["carry"][1], 4);

        let closed = vote_status(&mut world, &json!({})).expect("status");
        assert_eq!(closed["open"], false);
        crate::contract::assert_result_named("rules.vote_status", &closed);
        let err = vote_cast(&mut world, &json!({ "voter": 1, "option": 0 })).expect_err("no vote");
        assert_eq!(code(&err), Some("vote_closed"));
        {
            let mut rules = world.get_resource_mut::<MatchRules>().expect("rules");
            rules.session.maps.map_vote = true;
            rules.session.maps.rotation =
                vec!["mesa".into(), "canyon".into(), "ridge".into(), "flats".into()];
            rules
                .session
                .board
                .end_match(0, 2, regia_framework::rules::objective::EndReason::Ended);
            rules.session.step(&[]);
        }
        let open = vote_status(&mut world, &json!({})).expect("open");
        assert_eq!(open["options"], json!(["mesa", "canyon", "ridge"]));
        crate::contract::assert_result_named("rules.vote_status", &open);
        let err = vote_cast(
            &mut world,
            &json!({ "as": { "team": 1, "slot": 0 }, "voter": 1, "option": 0 }),
        )
        .expect_err("other voter");
        assert_eq!(code(&err), Some("match_not_permitted"));
        let cast = vote_cast(&mut world, &json!({ "voter": 256, "option": 0 })).expect("host cast");
        assert_eq!(cast["vote_count"], 1);
        crate::contract::assert_result_named("rules.vote_cast", &cast);
        let err = vote_cast(&mut world, &json!({ "voter": 1, "option": 9 })).expect_err("option");
        assert_eq!(code(&err), Some("vote_bad_option"));
        {
            let mut rules = world.get_resource_mut::<MatchRules>().expect("rules");
            rules.session.vote.as_mut().expect("vote").close_tick = 0;
        }
        let err = vote_cast(
            &mut world,
            &json!({
                "as": { "team": 1, "slot": 0 },
                "voter": 1,
                "option": 0,
            }),
        )
        .expect_err("guest on a closed vote, wrong voter");
        assert_eq!(code(&err), Some("match_not_permitted"));

        let stats = end_stats(
            &mut world,
            &json!({
                "note_kill": { "tick": 10, "killer_team": 1, "victim_team": 2 },
                "note_structure": { "tick": 12, "team": 2, "stem": "forge", "producer": true },
                "players": [{ "team": 1, "name": "ada", "kills": 7 }, { "team": 1, "name": "bot", "kills": 9, "is_ai": true }],
            }),
        )
        .expect("stats");
        assert_eq!(stats["lines"].as_array().map(Vec::len), Some(2));
        assert_eq!(stats["mvp"], "MVP  ADA (T1)  7 KILLS");
        crate::contract::assert_result_named("rules.end_stats", &stats);
        let err = end_stats(
            &mut world,
            &json!({
                "as": { "team": 1, "slot": 0 },
                "note_kill": { "tick": 20, "killer_team": 1, "victim_team": 2 },
            }),
        )
        .expect_err("guest note");
        assert_eq!(code(&err), Some("match_not_permitted"));
    }

    /// ADR-8647: the preview lists every start with its terms, in spawn order, and the pick.
    #[test]
    fn spawn_preview_lists_every_point_with_its_terms() {
        use regia_core::math::Vec3;
        use regia_framework::player::{PlayerStart, SpawnPoint};
        use regia_framework::rules::TeamId;
        use regia_scene::Transform;

        let mut world = World::new();
        let near = world
            .spawn((Transform::from_translation(Vec3::new(2.0, 0.0, 0.0)), PlayerStart))
            .id();
        let far = world
            .spawn((
                Transform::from_translation(Vec3::new(30.0, 0.0, 0.0)),
                PlayerStart,
            ))
            .id();
        let enemy_side = world
            .spawn((
                Transform::from_translation(Vec3::new(50.0, 0.0, 0.0)),
                PlayerStart,
                SpawnPoint::for_team(2),
            ))
            .id();
        world.spawn((Transform::default(), TeamId(2)));
        let player = world.spawn(TeamId(1)).id();
        match_setup(
            &mut world,
            &json!({ "settings": { "respawn": { "spawn": { "kind": "scored" } } } }),
        )
        .expect("setup");
        let out = spawn_preview(&mut world, &json!({ "player": player.to_bits() })).expect("preview");
        assert_eq!(out["policy"], json!("scored"));
        // No PlayerPlugin in this world: the walk is position order (ADR-8704).
        assert_eq!(out["order"], json!("position"));
        assert_eq!(out["team"], json!(1));
        assert_eq!(out["side"], json!(1));
        assert_eq!(out["pick"], json!(far.to_bits()));
        let points = out["points"].as_array().expect("points");
        let entities: Vec<u64> = points.iter().filter_map(|p| p["entity"].as_u64()).collect();
        assert_eq!(
            entities,
            vec![near.to_bits(), far.to_bits(), enemy_side.to_bits()]
        );
        for key in [
            "position",
            "team",
            "tags",
            "priority",
            "enabled",
            "eligible",
            "refused",
            "nearest_enemy_cm",
            "enemies_in_sight",
            "teammates_near",
            "recent_uses",
            "score",
        ] {
            assert!(points.iter().all(|p| p.get(key).is_some()), "{key}");
        }
        assert_eq!(points[0]["nearest_enemy_cm"], json!(200));
        assert_eq!(points[0]["enemies_in_sight"], json!(1));
        assert_eq!(points[2]["refused"], json!("wrong_team"));
        assert_eq!(points[2]["eligible"], json!(false));
        assert_eq!(out["respawn"]["spawn"]["kind"], json!("scored"));
        // The match_* answers keep their shape.
        let got = match_get(&mut world, &json!({})).expect("get");
        assert!(got.get("spawn").is_none() && got.get("respawns").is_none());
        // Errors: a missing entity and a bad parameter.
        let gone = world.spawn_empty().id();
        world.despawn(gone);
        let err = spawn_preview(&mut world, &json!({ "player": gone.to_bits() })).expect_err("gone");
        assert_eq!(code(&err), Some("spawn_player_missing"));
        crate::contract::assert_listed("rules.spawn_preview", "spawn_player_missing");
        let err = spawn_preview(&mut world, &json!({})).expect_err("no player");
        assert_eq!(code(&err), Some("F0002"));
    }

    /// A client has no session: `rules.match_get` answers from the replicated views.
    #[test]
    fn match_get_on_a_client_answers_from_the_replicated_views() {
        use regia_framework::rules::match_replication::{ObjectiveView, ObjectiveViewState};
        use regia_framework::rules::{
            MatchPhaseView, MatchStateEntity, ObjectiveViews, PlayerScoreView, TeamScoresView,
        };
        let mut world = World::new();
        let err = match_get(&mut world, &json!({})).expect_err("nothing to read");
        assert_eq!(code(&err), Some("match_rules_unavailable"));
        world.spawn((
            MatchStateEntity,
            MatchPhaseView {
                phase: 2,
                match_tick: 40,
                time_left_ticks: Some(60),
                net_tick: 100,
                ..MatchPhaseView::default()
            },
            TeamScoresView {
                scores: vec![0, 2, 1],
                ..TeamScoresView::default()
            },
            ObjectiveViews {
                items: vec![ObjectiveView {
                    id: 900,
                    team: 1,
                    state: ObjectiveViewState::Carried {
                        carrier: 7,
                        seat: Some(regia_framework::rules::match_replication::SeatView { team: 1, slot: 0 }),
                    },
                    ..ObjectiveView::default()
                }],
            },
        ));
        world.spawn(PlayerScoreView {
            team: 1,
            slot: 0,
            row_id: 65_793,
            kills: 3,
            score: 3,
            ..PlayerScoreView::default()
        });
        let got = match_get(&mut world, &json!({})).expect("replicated");
        assert_eq!(got["source"], "replicated");
        assert_eq!(got["phase"], "live");
        assert_eq!(got["time_left_ticks"], 60, "no net node: the written clock");
        assert_eq!(got["team_scores"], json!([0, 2, 1]));
        assert_eq!(got["objectives"][0]["state"], "carried");
        assert_eq!(got["objectives"][0]["carrier"], 7);
        assert_eq!(
            got["objectives"][0]["carrier_seat"],
            json!({ "team": 1, "slot": 0 })
        );
        assert_eq!(got["objectives"][0]["kind"], "flag");
        assert_eq!(got["players"][0]["kills"], 3);
        assert_eq!(got["players"][0]["local"], false);
        crate::contract::assert_result_named("rules.match_get", &got);
    }

    /// ADR-8695: `rules.match_get` shows co-op waves and race standings once their settings are
    /// on, and `rules.match_setup` refuses settings that cannot run.
    #[test]
    fn match_get_shows_waves_and_race_standings() {
        let mut world = World::new();
        let err = match_setup(
            &mut world,
            &json!({ "settings": { "waves": { "waves": [{ "groups": [] }] } } }),
        )
        .expect_err("empty wave");
        assert_eq!(code(&err), Some("match_wave_settings"));
        let err = match_setup(
            &mut world,
            &json!({ "settings": { "race": { "checkpoints": [
                { "id": 1, "volume": { "min": [-3.0, -1.0, -3.0], "max": [3.0, 3.0, 3.0] }, "next": [9] },
                { "id": 2, "volume": { "min": [97.0, -1.0, -3.0], "max": [103.0, 3.0, 3.0] }, "next": [1] }
            ] } } }),
        )
        .expect_err("unknown next");
        assert_eq!(code(&err), Some("match_race_settings"));
        let plain = match_setup(&mut world, &json!({ "start": true })).expect("plain");
        assert!(
            plain.get("waves").is_none() && plain.get("race").is_none(),
            "off by default"
        );

        match_setup(
            &mut world,
            &json!({
                "start": true,
                "settings": {
                    "waves": { "waves": [{ "groups": [{ "archetype": "grunt", "count": 3, "spawn_tag": "east" }] }] },
                    "race": { "laps": 2, "checkpoints": [
                        { "id": 1, "volume": { "min": [-3.0, -1.0, -3.0], "max": [3.0, 3.0, 3.0] }, "next": [2] },
                        { "id": 2, "volume": { "min": [97.0, -1.0, -3.0], "max": [103.0, 3.0, 3.0] }, "next": [1] }
                    ] }
                }
            }),
        )
        .expect("setup");
        match_assign(&mut world, &json!({ "team": 1, "slot": 0, "unit": 7 })).expect("seat");
        match_step(
            &mut world,
            &json!({ "units": [{ "id": 7, "team": 1, "position": [1.0, 0.0, 0.0] }] }),
        )
        .expect("step");
        let stepped = match_step(
            &mut world,
            &json!({ "units": [{ "id": 7, "team": 1, "position": [100.0, 0.0, 0.0] }] }),
        )
        .expect("step");
        assert!(
            stepped["events"]
                .as_array()
                .expect("events")
                .iter()
                .any(|e| e["kind"] == "racer_checkpoint" && e["checkpoint"] == 2),
            "{stepped}"
        );
        crate::contract::assert_result_named("rules.match_step", &stepped);
        let got = match_get(&mut world, &json!({})).expect("get");
        crate::contract::assert_result_named("rules.match_get", &got);
        assert_eq!(got["waves"]["alive"], 3);
        assert_eq!(got["waves"]["total"], 1);
        assert_eq!(got["waves"]["state"]["progress"][0]["released"], json!([3]));
        let standings = got["race"]["standings"].as_array().expect("standings");
        assert_eq!(standings.len(), 1);
        assert_eq!(standings[0]["seat"], json!({ "team": 1, "slot": 0 }));
        assert_eq!(standings[0]["position"], 1);
        assert_eq!(standings[0]["checkpoint_index"], 1);
    }

    /// GR-11.3/11.4: setup takes zone, elimination and victory settings; match_report feeds the
    /// host's counts; match_get shows zone, placements and victory; the surrender codes answer.
    #[test]
    fn match_report_and_get_show_zone_placements_and_victory() {
        let mut world = World::new();
        let bad = match_setup(
            &mut world,
            &json!({ "settings": { "zone": { "radius": 10.0, "phases": [{ "wait_ticks": 1, "shrink_ticks": 1, "end_radius": 20.0, "damage_per_tick": 1 }] } } }),
        )
        .expect_err("growing zone");
        assert_eq!(code(&bad), Some("match_shrink_zone_settings"));
        let bad = match_setup(
            &mut world,
            &json!({ "settings": { "victory": { "conditions": [{ "kind": "destroy_all_tagged", "tag": "" }] } } }),
        )
        .expect_err("empty tag");
        assert_eq!(code(&bad), Some("match_victory_settings"));
        let setup = match_setup(
            &mut world,
            &json!({ "start": true, "settings": {
                "flow": { "start_delay_ticks": 0 },
                "zone": { "radius": 50.0, "seed": 3, "phases": [{ "wait_ticks": 2, "shrink_ticks": 10, "end_radius": 5.0, "damage_per_tick": 2 }] },
                "victory": { "conditions": [{ "kind": "destroy_all_tagged", "tag": "capital" }] } } }),
        )
        .expect("setup");
        assert!(setup.get("zone").is_none(), "only match_get carries the zone");
        match_input(
            &mut world,
            &json!({ "inputs": [{ "input": "seat", "team": 1, "slot": 0 }, { "input": "seat", "team": 2, "slot": 0 }] }),
        )
        .expect("seats");
        let err = match_input(
            &mut world,
            &json!({ "input": { "input": "surrender_vote", "team": 1, "slot": 0, "yes": true } }),
        )
        .expect_err("no surrender condition");
        assert_eq!(code(&err), Some("match_surrender_off"));
        let reported = match_report(
            &mut world,
            &json!({ "tagged": [{ "team": 1, "tag": "capital", "count": 1 }, { "team": 2, "tag": "capital", "count": 1 }] }),
        )
        .expect("report");
        crate::contract::assert_result_named("rules.match_report", &reported);
        match_step(&mut world, &json!({ "steps": 3 })).expect("step");
        let got = match_get(&mut world, &json!({})).expect("get");
        crate::contract::assert_result_named("rules.match_get", &got);
        assert_eq!(got["zone"]["stage"], "shrinking");
        assert_eq!(got["zone"]["damage_per_tick"], 2);
        assert_eq!(got["placements"], json!([]));
        let err = match_report(
            &mut world,
            &json!({ "tagged": [{ "team": 1, "tag": "", "count": 0 }] }),
        )
        .expect_err("empty tag");
        assert_eq!(code(&err), Some("match_victory_settings"));
        match_report(
            &mut world,
            &json!({ "tagged": [{ "team": 1, "tag": "capital", "count": 0 }] }),
        )
        .expect("lost");
        let stepped = match_step(&mut world, &json!({ "steps": 1 })).expect("step");
        let kinds: Vec<&str> = stepped["events"]
            .as_array()
            .expect("events")
            .iter()
            .filter_map(|e| e["kind"].as_str())
            .collect();
        assert!(kinds.contains(&"team_eliminated"), "{kinds:?}");
        assert!(kinds.contains(&"victory"), "{kinds:?}");
        let got = match_get(&mut world, &json!({})).expect("get");
        assert_eq!(
            got["placements"],
            json!([{ "team": 2, "place": 1 }, { "team": 1, "place": 2 }])
        );
        assert_eq!(got["victory"]["winners"], json!([2]));
        assert_eq!(got["board"]["end_reason"], "destroyed");
    }

    /// rules-playlist-rotation-review finding 3: a player gate inverted through the
    /// `min_players` / `max_players` options (the bound the rotation's gate reads when the field
    /// is absent) is refused like an inverted field pair.
    #[test]
    fn match_setup_refuses_a_player_gate_inverted_through_options() {
        let mut world = World::new();
        for bad in [
            json!({ "playlist": [{ "map_id": "a", "mode": "ctf", "min_players": 8, "options": { "max_players": "2" } }] }),
            json!({ "playlist": [{ "map_id": "a", "mode": "ctf", "options": { "min_players": "9", "max_players": "2" } }] }),
        ] {
            let err = match_setup(&mut world, &bad).expect_err("refused");
            assert_eq!(code(&err), Some("F0002"), "{bad}");
        }
        // A consistent mixed pair still sets up.
        match_setup(
            &mut world,
            &json!({ "playlist": [{ "map_id": "a", "mode": "ctf", "min_players": 2, "options": { "max_players": "8" } }] }),
        )
        .expect("a gate of 2..=8 is fine");
    }

    /// ADR-8897: `rules.match_setup` takes the rotation order, the repeat guard, the playlist id
    /// and row gates; a gated row is reported as `row_skipped` before `launched`.
    #[test]
    fn match_setup_takes_the_rotation_and_reports_skipped_rows() {
        let mut world = World::new();
        let setup = match_setup(
            &mut world,
            &json!({
                "order": { "shuffle": { "seed": 7 } },
                "no_repeat_last": 2,
                "playlist_id": "public-ctf",
                "looping": true,
                "playlist": [
                    { "map_id": "harbor", "mode": "ctf", "weight": 2 },
                    { "map_id": "canyon", "mode": "ctf", "min_players": 8 },
                ],
            }),
        )
        .expect("setup");
        assert_eq!(setup["playlist"]["order"], json!({ "shuffle": { "seed": 7 } }));
        assert_eq!(setup["playlist"]["no_repeat_last"], 2);
        assert_eq!(setup["playlist"]["id"], "public-ctf");
        assert_eq!(setup["playlist"]["entries"][0]["weight"], 2);
        assert_eq!(setup["playlist"]["entries"][1]["min_players"], 8);
        crate::contract::assert_result_named("rules.match_setup", &setup);

        match_setup(
            &mut world,
            &json!({
                "playlist": [
                    { "map_id": "canyon", "mode": "ctf", "min_players": 8 },
                    { "map_id": "harbor", "mode": "koth" },
                ],
                "start": true,
            }),
        )
        .expect("setup");
        let stepped = match_step(&mut world, &json!({ "steps": 1 })).expect("step");
        let events = stepped["events"].as_array().expect("events");
        let skipped = events
            .iter()
            .position(|e| e["kind"] == "row_skipped")
            .expect("row_skipped");
        let launched = events
            .iter()
            .position(|e| e["kind"] == "launched")
            .expect("launched");
        assert!(skipped < launched);
        assert_eq!(events[skipped]["reason"], "too_few_players");
        assert_eq!(events[skipped]["map_id"], "canyon");
        assert_eq!(events[launched]["map_id"], "harbor");

        for bad in [
            json!({ "playlist": [{ "map_id": "a", "mode": "ctf", "weight": 101 }] }),
            json!({ "playlist": [{ "map_id": "a", "mode": "ctf", "min_players": 9, "max_players": 2 }] }),
            json!({ "order": "sideways" }),
            json!({ "no_repeat_last": 256 }),
        ] {
            let err = match_setup(&mut world, &bad).expect_err("refused");
            assert_eq!(code(&err), Some("F0002"), "{bad}");
        }
    }

    /// A `rules.match_report` that would pass the row cap changes nothing: the book says every
    /// table is checked before any row lands. The tagged table is already full, so an update of
    /// `t0000` followed by a new tag must be refused with both rows still as they were.
    #[test]
    fn a_report_that_crosses_the_row_cap_changes_nothing() {
        use regia_framework::rules::MatchRules;
        use regia_framework::rules::victory::MAX_REPORT_ROWS;

        let mut world = World::new();
        match_setup(
            &mut world,
            &json!({ "start": true, "settings": { "flow": { "max_teams": 2, "seats_per_team": 1, "start_delay_ticks": 0 } } }),
        )
        .expect("setup");
        {
            let mut rules = world.get_resource_mut::<MatchRules>().expect("rules");
            for i in 0..MAX_REPORT_ROWS {
                rules
                    .session
                    .report_tagged(1, &format!("t{i:04}"), 1)
                    .expect("fill");
            }
        }
        let err = match_report(
            &mut world,
            &json!({
                "tagged": [
                    { "team": 1, "tag": "t0000", "count": 9 },
                    { "team": 1, "tag": "brand-new", "count": 1 }
                ]
            }),
        )
        .expect_err("row cap");
        assert_eq!(code(&err), Some("match_victory_settings"));
        let got = match_get(&mut world, &json!({})).expect("get");
        let tagged = got["victory"]["state"]["tagged"].as_array().expect("tagged");
        assert_eq!(tagged.len(), MAX_REPORT_ROWS);
        let row = tagged.iter().find(|row| row["tag"] == "t0000").expect("t0000");
        assert_eq!(
            row["count"],
            json!(1),
            "the update must not land when a later row is refused"
        );
        assert!(tagged.iter().all(|row| row["tag"] != "brand-new"));
    }

    /// ADR-8802 (rules-genre-br-rts-review finding B, the agent mirror): two `rules.match_report`
    /// calls that defeat teams on one match tick place them together in team-id order.
    #[test]
    fn same_tick_defeats_from_two_reports_are_in_team_id_order() {
        let out = |first: u8, second: u8| {
            let mut world = World::new();
            match_setup(
                &mut world,
                &json!({ "start": true, "settings": {
                    "flow": { "max_teams": 3, "seats_per_team": 1, "start_delay_ticks": 0 },
                    "objectives": { "max_teams": 3 },
                    "elimination": { "last_standing": true } } }),
            )
            .expect("setup");
            match_input(
                &mut world,
                &json!({ "inputs": [
                    { "input": "seat", "team": 1, "slot": 0 },
                    { "input": "seat", "team": 2, "slot": 0 },
                    { "input": "seat", "team": 3, "slot": 0 }] }),
            )
            .expect("seats");
            match_report(&mut world, &json!({ "defeat": [first] })).expect("first");
            let got = match_report(&mut world, &json!({ "defeat": [second] })).expect("second");
            got["victory"]["out"]
                .as_array()
                .expect("out")
                .iter()
                .map(|row| (row["team"].clone(), row["place"].clone()))
                .collect::<Vec<_>>()
        };
        let expected = vec![(json!(1), json!(3)), (json!(2), json!(2))];
        assert_eq!(out(2, 1), expected, "defeat 2 then 1");
        assert_eq!(out(1, 2), expected, "defeat 1 then 2");
    }
}
