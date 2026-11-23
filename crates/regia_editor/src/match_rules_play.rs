// SPDX-License-Identifier: MPL-2.0
//! Play-in-editor match controls, the live match view and the match-rules overlays (ADR-8738,
//! `book/agents/editor-match-rules.md`).
//!
//! - **Match strip.** The Match Rules panel and the play toolbar's Match menu draw the same strip:
//!   start now, pause, resume, end the match, step N ticks while paused, add or remove a bot seat per
//!   team and force the next playlist row. Every control calls [`match_control_method`], the handler
//!   of `editor.match_control`, so a click and an agent call are the same request. Requests are
//!   queued in the edit world and applied to the play world's `MatchRules` session by [`pump`] on
//!   the editor's next frame; the outcome is `match.last_control` of `editor.match_rules_state`.
//!   `start` on a play world without a session first creates one from the level's rule set
//!   (`LevelSettings.game_rules` resolved through the project's library, else team deathmatch) and
//!   the open playlist's rows. A step while paused is a host resume, N session steps over the play
//!   world's contestants, and a host pause again, in one request (the events include `resumed` and
//!   `paused`). A bot seat is a seated slot with no pawn: it counts for teams and ready checks.
//! - **Live view.** Each frame [`pump`] mirrors the play session (tick, phase, scores, the board's
//!   objectives) and, when the play world has a player, the spawn picker's scores
//!   (`regia_framework::spawn_select::spawn_preview`, what `rules.spawn_preview` answers) into the
//!   edit world, where the overlays and the panel read them.
//! - **Overlays** (Show ▸ Icons ▸ Match rules, beside Player starts): spawn points tinted by their
//!   team's color with a team and priority label, flag bases with their touch radius and height
//!   band, hill and zone boxes, zone capture progress rings, and during play the spawn-score heat.
//!   The board comes from the live view during play, else from the edit world's own `MatchRules`.

use std::collections::BTreeMap;

use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_core::math::{Vec2, Vec3};
use regia_ecs::prelude::*;
use regia_framework::rules::MatchRules;
use regia_framework::rules::match_flow::{FlowInput, SeatId};
use regia_framework::rules::mode::MatchMode;
use regia_framework::rules::objective::{Objective, ObjectiveKind};
use regia_framework::rules::playlist::PlaylistEntry;
use regia_framework::rules::ruleset::ResolvedRules;
use regia_framework::rules::session::MatchSession;
use regia_framework::rules::teams::{TeamDef, default_team_defs};
use serde_json::{Value, json};

use crate::match_rules_panel::{MatchRulesPanel, codes, project_rules};
use crate::overlays::OverlayShape;
use crate::views::ViewCamera;

/// Most ticks one `step` runs.
pub const MAX_STEP_TICKS: u64 = 100_000;

/// One match control (`editor.match_control { action }`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchAction {
    /// Start the next playlist row now (creating the session when the play world has none).
    Start,
    /// The host pause.
    Pause,
    /// The host resume.
    Resume,
    /// End the match (the flow waits for every seat to ready).
    End,
    /// Run this many ticks while paused.
    Step(u64),
    /// Seat a bot on this team.
    AddBot(u8),
    /// Unseat this team's last bot.
    RemoveBot(u8),
    /// The playlist row the next launch starts.
    NextRow(u32),
}

impl MatchAction {
    /// The protocol word.
    #[must_use]
    pub fn word(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Pause => "pause",
            Self::Resume => "resume",
            Self::End => "end",
            Self::Step(_) => "step",
            Self::AddBot(_) => "add_bot",
            Self::RemoveBot(_) => "remove_bot",
            Self::NextRow(_) => "next_row",
        }
    }
}

/// What the last applied control did.
#[derive(Debug, Clone, PartialEq)]
pub struct ControlOutcome {
    /// The request id `editor.match_control` answered.
    pub id: u64,
    /// The action word.
    pub action: &'static str,
    /// The refusal code (`match_control_*`, or the rules' own `match_*` code), `None` when applied.
    pub code: Option<String>,
    /// What happened.
    pub message: String,
    /// The session's events of the control (JSON of `RulesEvent`).
    pub events: Vec<Value>,
    /// The match tick after it.
    pub tick: u64,
}

/// One spawn point's score in the live view.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpawnHeat {
    /// Where the point is.
    pub position: Vec3,
    /// The picker's score.
    pub score: i64,
    /// Whether the picker could choose it.
    pub eligible: bool,
}

/// The play session as the edit world sees it (mirrored each frame by [`pump`]).
#[derive(Debug, Clone, PartialEq)]
pub struct MatchLive {
    /// Match tick.
    pub tick: u64,
    /// Phase word.
    pub phase: &'static str,
    /// Host or friendly pause.
    pub paused: bool,
    /// Between matches (waiting for ready).
    pub between: bool,
    /// The board's end flag.
    pub over: bool,
    /// The board's mode lobby id.
    pub mode: &'static str,
    /// Score per team (index = team id).
    pub team_scores: Vec<u32>,
    /// The board's objectives.
    pub objectives: Vec<Objective>,
    /// Flag touch radius.
    pub touch_radius: f32,
    /// Flag touch height band.
    pub touch_height: Option<f32>,
    /// The team definitions.
    pub teams: Vec<TeamDef>,
    /// The next playlist row and the row count.
    pub playlist: (u32, usize),
    /// Seated seats.
    pub seated: Vec<SeatId>,
    /// The spawn picker's scores for the first player (empty without a player).
    pub heat: Vec<SpawnHeat>,
}

/// The match controls' state (edit-world resource).
#[derive(Resource, Debug, Clone, Default)]
pub struct MatchPlayState {
    /// Queued controls, oldest first.
    pub queue: Vec<(u64, MatchAction)>,
    /// The id the next control gets.
    pub next_id: u64,
    /// The last applied control.
    pub last_control: Option<ControlOutcome>,
    /// The live view while a session runs.
    pub live: Option<MatchLive>,
    /// Bot seats the strip added this session.
    pub bots: Vec<SeatId>,
    /// The strip's step count.
    pub strip_steps: u32,
    /// The strip's team.
    pub strip_team: u8,
    /// The strip's playlist row.
    pub strip_row: u32,
    /// Team colors of the level's rule set (cache) and the key they were resolved for.
    team_colors: Vec<TeamDef>,
    team_colors_key: Option<(Option<String>, u64)>,
}

fn refused(code: &str, message: impl Into<String>, hint: impl Into<String>) -> RpcError {
    let mut error = RpcError::new(INVALID_PARAMS, code, message, hint);
    if let Some(Value::Object(data)) = error.data.as_mut() {
        data.insert("reason".into(), json!(code));
    }
    error
}

fn number(params: &Value, key: &str, action: &str) -> Result<u64, RpcError> {
    params.get(key).and_then(Value::as_u64).ok_or_else(|| {
        refused(
            codes::MATCH_BAD_ACTION,
            format!("`{action}` needs `{key}` (a whole number)"),
            "see book/agents/editor-match-rules.md",
        )
    })
}

/// Parses `editor.match_control`'s parameters.
///
/// # Errors
///
/// [`codes::MATCH_BAD_ACTION`].
pub fn parse_action(params: &Value) -> Result<MatchAction, RpcError> {
    let action = params.get("action").and_then(Value::as_str).unwrap_or_default();
    let small = |key: &str| -> Result<u8, RpcError> {
        u8::try_from(number(params, key, action)?).map_err(|_| {
            refused(
                codes::MATCH_BAD_ACTION,
                format!("`{key}` is a team id (1..=255)"),
                "e.g. team: 1",
            )
        })
    };
    Ok(match action {
        "start" => MatchAction::Start,
        "pause" => MatchAction::Pause,
        "resume" => MatchAction::Resume,
        "end" => MatchAction::End,
        "step" => {
            let steps = number(params, "steps", action)?;
            if !(1..=MAX_STEP_TICKS).contains(&steps) {
                return Err(refused(
                    codes::MATCH_BAD_ACTION,
                    format!("`steps` must be 1..={MAX_STEP_TICKS}"),
                    "e.g. steps: 60",
                ));
            }
            MatchAction::Step(steps)
        }
        "add_bot" => MatchAction::AddBot(small("team")?),
        "remove_bot" => MatchAction::RemoveBot(small("team")?),
        "next_row" => MatchAction::NextRow(u32::try_from(number(params, "row", action)?).map_err(|_| {
            refused(
                codes::MATCH_BAD_ACTION,
                "`row` is too large",
                "a playlist row index",
            )
        })?),
        other => {
            return Err(refused(
                codes::MATCH_BAD_ACTION,
                format!("unknown action `{other}`"),
                "start, pause, resume, end, step, add_bot, remove_bot or next_row",
            ));
        }
    })
}

/// `editor.match_control`: queue one control for the play world.
///
/// # Errors
///
/// [`codes::MATCH_BAD_ACTION`], or [`codes::MATCH_NOT_PLAYING`] while no session runs.
pub fn match_control_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let action = parse_action(params)?;
    let playing = world
        .get_resource::<crate::pie::PieStatus>()
        .is_some_and(|status| status.mode.is_some());
    if !playing {
        return Err(refused(
            codes::MATCH_NOT_PLAYING,
            "no play session runs",
            "start one with editor.play",
        ));
    }
    let mut state = world.get_resource_or_insert_with(MatchPlayState::default);
    state.next_id += 1;
    let id = state.next_id;
    state.queue.push((id, action));
    Ok(json!({ "queued": true, "id": id, "action": action.word() }))
}

/// The session `start` creates on a play world without one: the level's rule set (else team
/// deathmatch) over the open playlist's rows (else one row of that mode).
///
/// Refused with the row's `ruleset_*` code when a row names a rule set it does not resolve
/// against (an option the schema does not declare, or a value out of range): the same rows the
/// playlist's save refuses never start.
fn seed_session(edit: &mut World) -> Result<MatchSession, (String, String)> {
    let level = regia_scene::level::current_settings(edit);
    let playlist = edit
        .get_resource::<MatchRulesPanel>()
        .and_then(|p| p.playlist.as_ref().map(|doc| doc.file.clone()));
    let library = project_rules(edit, false).library().clone();
    let resolved = level
        .game_rules
        .as_deref()
        .and_then(|name| library.resolve(name, &BTreeMap::new()).ok())
        .unwrap_or_else(|| ResolvedRules::skeleton(MatchMode::TeamDeathmatch));
    let mut session = resolved.session();
    let (rows, looping) = match playlist {
        Some(file) if !file.entries.is_empty() => (file.entries, file.looping),
        _ => (vec![PlaylistEntry::new("", resolved.id.clone())], false),
    };
    session.rule_modes = library.rule_modes(&rows).map_err(|error| {
        (
            error.code.clone(),
            format!("the playlist does not start: {error}"),
        )
    })?;
    session.playlist.set(rows);
    session.playlist.looping = looping;
    Ok(session)
}

fn rules_refusal(error: &regia_framework::rules::RulesError) -> (String, String) {
    (error.code().to_owned(), error.to_string())
}

/// Applies one control to the play world. `seed` builds a session for `start` when there is none.
fn apply_control(
    play: &mut World,
    state: &mut MatchPlayState,
    action: MatchAction,
    seed: &mut dyn FnMut() -> Result<MatchSession, (String, String)>,
) -> Result<(String, Vec<Value>), (String, String)> {
    let no_session = || {
        (
            codes::MATCH_NO_SESSION.to_owned(),
            "the play world has no match session: start one first".to_owned(),
        )
    };
    if action == MatchAction::Start && !play.contains_resource::<MatchRules>() {
        play.insert_resource(MatchRules { session: seed()? });
    }
    let units: Vec<_> = if matches!(action, MatchAction::Step(_)) {
        regia_framework::rules::match_contestants(play)
            .into_iter()
            .map(|(unit, _)| unit)
            .collect()
    } else {
        Vec::new()
    };
    let mut rules = play.get_resource_mut::<MatchRules>().ok_or_else(no_session)?;
    let session = &mut rules.session;
    let message = match action {
        MatchAction::Start => {
            session.start_now();
            "started the next row".to_owned()
        }
        MatchAction::Pause => {
            session
                .apply(&FlowInput::HostPause { on: true })
                .map_err(|e| rules_refusal(&e))?;
            "paused".to_owned()
        }
        MatchAction::Resume => {
            session
                .apply(&FlowInput::HostPause { on: false })
                .map_err(|e| rules_refusal(&e))?;
            "resumed".to_owned()
        }
        MatchAction::End => {
            session
                .apply(&FlowInput::OpenBetween)
                .map_err(|e| rules_refusal(&e))?;
            "ended the match".to_owned()
        }
        MatchAction::Step(steps) => {
            if !session.flow.paused {
                return Err((
                    codes::MATCH_NOT_PAUSED.to_owned(),
                    "step runs while the match is paused: pause it first".to_owned(),
                ));
            }
            session
                .apply(&FlowInput::HostPause { on: false })
                .map_err(|e| rules_refusal(&e))?;
            let mut stepped = Vec::new();
            for _ in 0..steps {
                stepped.extend(session.step(&units).events);
            }
            session
                .apply(&FlowInput::HostPause { on: true })
                .map_err(|e| rules_refusal(&e))?;
            let mut events: Vec<Value> = session
                .drain_events()
                .into_iter()
                .chain(stepped)
                .filter_map(|e| serde_json::to_value(e).ok())
                .collect();
            events.sort_by_key(|e| e.get("tick").and_then(Value::as_u64).unwrap_or(0));
            regia_framework::rules::retag_moved_seats(play);
            return Ok((format!("stepped {steps} tick(s)"), events));
        }
        MatchAction::AddBot(team) => {
            let settings = session.flow.settings;
            if team == 0 || team > settings.max_teams {
                return Err((
                    codes::MATCH_BAD_ACTION.to_owned(),
                    format!("team {team} is not a fighting team (1..={})", settings.max_teams),
                ));
            }
            let slot = (0..settings.seats_per_team)
                .find(|slot| !session.flow.seated.contains(&SeatId::new(team, *slot)))
                .ok_or_else(|| {
                    (
                        codes::MATCH_NO_SEAT.to_owned(),
                        format!("team {team} has no free seat"),
                    )
                })?;
            session
                .apply(&FlowInput::Seat { team, slot })
                .map_err(|e| rules_refusal(&e))?;
            state.bots.push(SeatId::new(team, slot));
            format!("seated a bot on team {team} slot {slot}")
        }
        MatchAction::RemoveBot(team) => {
            let index = state
                .bots
                .iter()
                .rposition(|seat| seat.team == team)
                .ok_or_else(|| {
                    (
                        codes::MATCH_NO_BOT.to_owned(),
                        format!("team {team} has no bot seat"),
                    )
                })?;
            let seat = state.bots[index];
            session
                .apply(&FlowInput::Leave {
                    team: seat.team,
                    slot: seat.slot,
                })
                .map_err(|e| rules_refusal(&e))?;
            state.bots.remove(index);
            format!("removed the bot on team {team} slot {}", seat.slot)
        }
        MatchAction::NextRow(row) => {
            let len = session.playlist.entries.len();
            if usize::try_from(row).unwrap_or(usize::MAX) >= len {
                return Err((
                    codes::MATCH_BAD_ROW.to_owned(),
                    format!("row {row} is past the playlist's {len} rows"),
                ));
            }
            session.playlist.index = row;
            format!("the next launch starts row {row}")
        }
    };
    let events = session
        .drain_events()
        .into_iter()
        .filter_map(|e| serde_json::to_value(e).ok())
        .collect();
    Ok((message, events))
}

fn mirror(play: &mut World) -> Option<MatchLive> {
    let heat = {
        let mut players: Vec<Entity> = play
            .query_filtered::<Entity, With<regia_framework::player::Player>>()
            .iter(play)
            .collect();
        players.sort_by_key(|e| (e.index().index(), e.to_bits()));
        match players.first() {
            Some(player) if play.contains_resource::<MatchRules>() => {
                regia_framework::spawn_select::spawn_preview(play, *player)
                    .rows
                    .iter()
                    .map(|row| SpawnHeat {
                        position: row.position,
                        score: row.terms.score,
                        eligible: row.terms.refused.is_none(),
                    })
                    .collect()
            }
            _ => Vec::new(),
        }
    };
    let session = &play.get_resource::<MatchRules>()?.session;
    Some(MatchLive {
        tick: session.flow.match_tick,
        phase: session.flow.phases.phase.as_str(),
        paused: session.flow.paused,
        between: session.flow.between,
        over: session.board.match_over,
        mode: session.board.mode.as_lobby_str(),
        team_scores: session.board.team_scores.clone(),
        objectives: session.board.objectives.clone(),
        touch_radius: session.board.settings.flag_touch_radius,
        touch_height: session.board.settings.touch_height,
        teams: session.teams.defs.clone(),
        playlist: (session.playlist.index, session.playlist.entries.len()),
        seated: session.flow.seated.clone(),
        heat,
    })
}

/// Refreshes the cached team colors of the level's rule set when the level's rules or the project
/// library changed.
fn refresh_team_colors(edit: &mut World) {
    let name = regia_scene::level::current_settings(edit).game_rules;
    let generation = edit
        .get_resource::<crate::match_rules_panel::ProjectRules>()
        .map_or(0, |p| p.generation);
    let key = Some((name.clone(), generation));
    if edit
        .get_resource::<MatchPlayState>()
        .is_some_and(|s| s.team_colors_key == key)
    {
        return;
    }
    let teams = name
        .as_deref()
        .and_then(|n| project_rules(edit, false).resolve(n, &BTreeMap::new()).ok())
        .map_or_else(default_team_defs, |resolved| resolved.settings.teams.defs);
    let mut state = edit.get_resource_or_insert_with(MatchPlayState::default);
    state.team_colors = teams;
    state.team_colors_key = key;
}

/// Each editor frame: applies the queued match controls to the play world and mirrors the live
/// session into the edit world.
pub fn pump(edit: &mut World, session: Option<&mut crate::pie::PieSession>) {
    let _span = tracing::trace_span!(target: "regia_editor", "match_rules_pump").entered();
    refresh_team_colors(edit);
    let Some(mut state) = edit.remove_resource::<MatchPlayState>() else {
        return;
    };
    match session {
        None => {
            for (id, action) in std::mem::take(&mut state.queue) {
                state.last_control = Some(ControlOutcome {
                    id,
                    action: action.word(),
                    code: Some(codes::MATCH_NOT_PLAYING.to_owned()),
                    message: "the play session ended before the control ran".to_owned(),
                    events: Vec::new(),
                    tick: 0,
                });
            }
            state.live = None;
            state.bots.clear();
        }
        Some(session) => {
            let queue = std::mem::take(&mut state.queue);
            let play = session.app.world_mut();
            for (id, action) in queue {
                // Built only when a `start` finds no session; a refused seed inserts nothing.
                let mut seed = || seed_session(edit);
                let result = apply_control(play, &mut state, action, &mut seed);
                let tick = play
                    .get_resource::<MatchRules>()
                    .map_or(0, |r| r.session.flow.match_tick);
                state.last_control = Some(match result {
                    Ok((message, events)) => ControlOutcome {
                        id,
                        action: action.word(),
                        code: None,
                        message,
                        events,
                        tick,
                    },
                    Err((code, message)) => ControlOutcome {
                        id,
                        action: action.word(),
                        code: Some(code),
                        message,
                        events: Vec::new(),
                        tick,
                    },
                });
            }
            state.live = mirror(play);
            let names: Vec<String> = play
                .get_resource::<regia_framework::rules::GameRulesRegistry>()
                .map(|r| r.registered_names().into_iter().map(str::to_owned).collect())
                .unwrap_or_default();
            if !names.is_empty()
                && let Some(mut panel) = edit.get_resource_mut::<MatchRulesPanel>()
                && panel.play_rule_names != names
            {
                panel.play_rule_names = names;
            }
        }
    }
    edit.insert_resource(state);
}

fn outcome_json(outcome: &ControlOutcome) -> Value {
    json!({
        "id": outcome.id,
        "action": outcome.action,
        "ok": outcome.code.is_none(),
        "code": outcome.code,
        "message": outcome.message,
        "events": outcome.events,
        "tick": outcome.tick,
    })
}

fn live_json(live: &MatchLive) -> Value {
    json!({
        "tick": live.tick,
        "phase": live.phase,
        "paused": live.paused,
        "between": live.between,
        "over": live.over,
        "mode": live.mode,
        "team_scores": live.team_scores,
        "objectives": live.objectives.iter().map(|o| json!({
            "id": o.entity_id,
            "kind": o.kind.as_str(),
            "team": o.team_id,
            "at_base": o.at_base,
            "carrier": o.carrier_id,
            "position": o.position,
            "owner": o.zone.map(|z| z.owner),
            "capturing": o.zone.map(|z| z.capturing_team),
            "progress": o.zone.map(|z| z.progress),
            "capture_ticks": o.zone.map(|z| z.settings.effective_capture_ticks()),
        })).collect::<Vec<_>>(),
        "playlist_index": live.playlist.0,
        "playlist_rows": live.playlist.1,
        "seated": live.seated.iter().map(|s| json!({ "team": s.team, "slot": s.slot })).collect::<Vec<_>>(),
        "spawn_heat": live.heat.iter().map(|h| json!({
            "position": h.position.to_array(),
            "score": h.score,
            "eligible": h.eligible,
        })).collect::<Vec<_>>(),
    })
}

/// `match` of `editor.match_rules_state`.
pub fn match_json(world: &World) -> Value {
    let playing = world
        .get_resource::<crate::pie::PieStatus>()
        .is_some_and(|status| status.mode.is_some());
    let state = world.get_resource::<MatchPlayState>();
    json!({
        "playing": playing,
        "session": state.is_some_and(|s| s.live.is_some()),
        "live": state.and_then(|s| s.live.as_ref()).map(live_json),
        "bots": state.map_or_else(Vec::new, |s| s.bots.iter().map(|b| json!({ "team": b.team, "slot": b.slot })).collect()),
        "last_control": state.and_then(|s| s.last_control.as_ref()).map(outcome_json),
        "queued": state.map_or(0, |s| s.queue.len()),
    })
}

// ---------------------------------------------------------------------------------------------
// The Match strip
// ---------------------------------------------------------------------------------------------

/// The Match strip (the Match Rules panel and the play toolbar's Match menu). Every control calls
/// [`match_control_method`], the handler of `editor.match_control`.
pub fn match_strip(ui: &mut egui::Ui, world: &mut World) {
    let playing = world
        .get_resource::<crate::pie::PieStatus>()
        .is_some_and(|status| status.mode.is_some());
    ui.strong("Match");
    if !playing {
        ui.weak("Start play to drive a match from here.");
        return;
    }
    let mut state = world.remove_resource::<MatchPlayState>().unwrap_or_default();
    state.strip_steps = state.strip_steps.max(1);
    state.strip_team = state.strip_team.max(1);
    let mut request: Option<Value> = None;
    ui.horizontal_wrapped(|ui| {
        for (label, action) in [
            ("Start now", "start"),
            ("Pause", "pause"),
            ("Resume", "resume"),
            ("End match", "end"),
        ] {
            if ui.button(label).clicked() {
                request = Some(json!({ "action": action }));
            }
        }
        ui.separator();
        ui.add(
            egui::DragValue::new(&mut state.strip_steps)
                .range(1..=10_000)
                .suffix(" ticks"),
        );
        if ui
            .button("Step")
            .on_hover_text("Run the ticks while paused")
            .clicked()
        {
            request = Some(json!({ "action": "step", "steps": state.strip_steps }));
        }
        ui.separator();
        ui.add(
            egui::DragValue::new(&mut state.strip_team)
                .range(1..=16)
                .prefix("team "),
        );
        if ui.button("+ Bot").clicked() {
            request = Some(json!({ "action": "add_bot", "team": state.strip_team }));
        }
        if ui.button("− Bot").clicked() {
            request = Some(json!({ "action": "remove_bot", "team": state.strip_team }));
        }
        ui.separator();
        ui.add(
            egui::DragValue::new(&mut state.strip_row)
                .range(0..=1023)
                .prefix("row "),
        );
        if ui.button("Next row").clicked() {
            request = Some(json!({ "action": "next_row", "row": state.strip_row }));
        }
    });
    if let Some(live) = &state.live {
        ui.label(format!(
            "tick {} · {} · {}{} · scores {:?}",
            live.tick,
            live.mode,
            live.phase,
            if live.paused { " (paused)" } else { "" },
            live.team_scores.get(1..).unwrap_or(&[]),
        ));
    } else {
        ui.weak("No match session in the play world yet: Start now creates one from the level's rules.");
    }
    if let Some(last) = &state.last_control {
        match &last.code {
            Some(code) => ui.colored_label(
                egui::Color32::from_rgb(230, 90, 80),
                format!("{code}: {}", last.message),
            ),
            None => ui.weak(&last.message),
        };
    }
    world.insert_resource(state);
    if let Some(params) = request
        && let Err(error) = match_control_method(world, &params)
        && let Some(mut panel) = world.get_resource_mut::<MatchRulesPanel>()
    {
        panel.status = error.message;
    }
}

// ---------------------------------------------------------------------------------------------
// Overlays
// ---------------------------------------------------------------------------------------------

fn color32(rgba: [u8; 4]) -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(rgba[0], rgba[1], rgba[2], 255)
}

/// The team definitions the overlays color by: the play session's, else the edit world's session,
/// else the level's rule set (cached by [`pump`]), else the default teams.
fn team_defs(world: &World) -> Vec<TeamDef> {
    let state = world.get_resource::<MatchPlayState>();
    if let Some(live) = state.and_then(|s| s.live.as_ref()) {
        return live.teams.clone();
    }
    if let Some(rules) = world.get_resource::<MatchRules>() {
        return rules.session.teams.defs.clone();
    }
    match state {
        Some(s) if !s.team_colors.is_empty() => s.team_colors.clone(),
        _ => default_team_defs(),
    }
}

/// The overlay color of fighting team `team` (`None` for team 0, which serves every team).
#[must_use]
pub fn team_color(world: &World, team: u8) -> Option<egui::Color32> {
    if team == 0 {
        return None;
    }
    let defs = team_defs(world);
    Some(
        defs.iter()
            .find(|d| d.id == team)
            .map_or(egui::Color32::from_rgb(200, 200, 200), |d| color32(d.color)),
    )
}

/// A player start's tint and label under the match-rules toggle: its `SpawnPoint` team's color and
/// `team N · pP` (`None` for a start without a team).
#[must_use]
pub fn player_start_tint(world: &World, entity: Entity) -> Option<(egui::Color32, String)> {
    let point = world.get::<regia_framework::player::SpawnPoint>(entity)?;
    let color = team_color(world, point.team)?;
    let mut label = format!("team {} · p{}", point.team, point.priority);
    if !point.enabled {
        label.push_str(" · off");
    }
    Some((color, label))
}

const NEUTRAL: egui::Color32 = egui::Color32::from_rgb(235, 235, 235);
const REFUSED: egui::Color32 = egui::Color32::from_rgb(110, 110, 110);

fn box_edges(min: Vec3, max: Vec3) -> [(Vec3, Vec3); 12] {
    let c = |x: bool, y: bool, z: bool| {
        Vec3::new(
            if x { max.x } else { min.x },
            if y { max.y } else { min.y },
            if z { max.z } else { min.z },
        )
    };
    [
        (c(false, false, false), c(true, false, false)),
        (c(false, false, true), c(true, false, true)),
        (c(false, true, false), c(true, true, false)),
        (c(false, true, true), c(true, true, true)),
        (c(false, false, false), c(false, true, false)),
        (c(true, false, false), c(true, true, false)),
        (c(false, false, true), c(false, true, true)),
        (c(true, false, true), c(true, true, true)),
        (c(false, false, false), c(false, false, true)),
        (c(true, false, false), c(true, false, true)),
        (c(false, true, false), c(false, true, true)),
        (c(true, true, false), c(true, true, true)),
    ]
}

/// An arc of `fraction` of a full turn around `center` in the XZ plane.
fn push_arc(
    out: &mut Vec<OverlayShape>,
    view: &ViewCamera,
    size: Vec2,
    center: Vec3,
    radius: f32,
    fraction: f32,
    color: egui::Color32,
) {
    let segments = 40usize;
    let lit = (fraction.clamp(0.0, 1.0) * segments as f32).round() as usize;
    for i in 0..lit {
        let a0 = std::f32::consts::TAU * i as f32 / segments as f32;
        let a1 = std::f32::consts::TAU * (i + 1) as f32 / segments as f32;
        crate::overlays::push_line(
            out,
            view,
            size,
            center + Vec3::new(a0.cos(), 0.0, a0.sin()) * radius,
            center + Vec3::new(a1.cos(), 0.0, a1.sin()) * radius,
            color,
        );
    }
}

fn label(
    out: &mut Vec<OverlayShape>,
    view: &ViewCamera,
    size: Vec2,
    at: Vec3,
    text: String,
    color: egui::Color32,
) {
    if let Some(p) = view.project(at, size)
        && (view.eye - at).dot(view.forward()) < 0.0
    {
        out.push(OverlayShape::Text {
            at: p + Vec2::new(8.0, -10.0),
            text,
            color,
        });
    }
}

/// The match-rules overlay of the objectives and the spawn heat (the spawn tint is drawn with the
/// player start icons).
#[must_use]
pub fn overlays(world: &World, view: &ViewCamera, size: Vec2) -> Vec<OverlayShape> {
    let _span = tracing::trace_span!(target: "regia_editor", "match_rules_overlay").entered();
    let mut out = Vec::new();
    let state = world.get_resource::<MatchPlayState>();
    let live = state.and_then(|s| s.live.as_ref());
    let (objectives, radius, height) = match (live, world.get_resource::<MatchRules>()) {
        (Some(live), _) => (live.objectives.clone(), live.touch_radius, live.touch_height),
        (None, Some(rules)) => (
            rules.session.board.objectives.clone(),
            rules.session.board.settings.flag_touch_radius,
            rules.session.board.settings.touch_height,
        ),
        (None, None) => (Vec::new(), 0.0, None),
    };
    let tint = |team: u8| team_color(world, team).unwrap_or(NEUTRAL);
    for objective in &objectives {
        match objective.kind {
            ObjectiveKind::Flag => {
                let base = Vec3::from_array(objective.base_position);
                let color = tint(objective.team_id);
                let (u, v) = (Vec3::X * radius, Vec3::Z * radius);
                crate::overlays::push_world_circle(&mut out, view, size, base, u, v, color);
                if let Some(h) = height {
                    for y in [h, -h] {
                        let ring = base + Vec3::Y * y;
                        crate::overlays::push_world_circle(&mut out, view, size, ring, u, v, color);
                    }
                    for dir in [Vec3::X, Vec3::NEG_X, Vec3::Z, Vec3::NEG_Z] {
                        let edge = base + dir * radius;
                        crate::overlays::push_line(
                            &mut out,
                            view,
                            size,
                            edge - Vec3::Y * h,
                            edge + Vec3::Y * h,
                            color,
                        );
                    }
                }
                let now = Vec3::from_array(objective.position);
                if !objective.at_base {
                    crate::overlays::push_line(&mut out, view, size, base, now, color);
                    crate::overlays::push_world_circle(
                        &mut out,
                        view,
                        size,
                        now,
                        Vec3::X * 0.3,
                        Vec3::Z * 0.3,
                        color,
                    );
                }
                let state_word = if objective.at_base {
                    "home"
                } else if objective.carrier_id.is_some() {
                    "carried"
                } else {
                    "dropped"
                };
                label(
                    &mut out,
                    view,
                    size,
                    base,
                    format!(
                        "flag {} · team {} · {state_word}",
                        objective.entity_id, objective.team_id
                    ),
                    color,
                );
            }
            ObjectiveKind::Hill | ObjectiveKind::Zone => {
                let Some(volume) = objective.volume else {
                    continue;
                };
                let (min, max) = (Vec3::from_array(volume.min), Vec3::from_array(volume.max));
                let owner = objective.zone.map_or(0, |z| z.owner);
                let color = tint(owner);
                for (a, b) in box_edges(min, max) {
                    crate::overlays::push_line(&mut out, view, size, a, b, color);
                }
                let center = Vec3::new((min.x + max.x) * 0.5, max.y, (min.z + max.z) * 0.5);
                let mut text = format!("{} {}", objective.kind.as_str(), objective.entity_id);
                if let Some(zone) = objective.zone {
                    let ring = ((max.x - min.x).min(max.z - min.z) * 0.5).max(0.25);
                    let total = zone.settings.effective_capture_ticks();
                    if zone.progress > 0 && zone.capturing_team != 0 {
                        // Ticks as a fraction of the capture time (display only).
                        let fraction = zone.progress as f32 / total as f32;
                        push_arc(
                            &mut out,
                            view,
                            size,
                            center,
                            ring,
                            fraction,
                            tint(zone.capturing_team),
                        );
                    }
                    text.push_str(&format!(" · owner {} · {}/{}", zone.owner, zone.progress, total));
                }
                label(&mut out, view, size, center, text, color);
            }
            // A kind a later core adds draws nothing until the editor knows its shape.
            _ => {}
        }
    }
    if let Some(live) = live {
        let scores: Vec<i64> = live.heat.iter().filter(|h| h.eligible).map(|h| h.score).collect();
        let (lo, hi) = (
            scores.iter().copied().min().unwrap_or(0),
            scores.iter().copied().max().unwrap_or(0),
        );
        for heat in &live.heat {
            let color = if heat.eligible {
                // In i128: a saturating `Scored` weight makes legal scores of i64::MIN and
                // i64::MAX, whose span does not fit an i64.
                let t = if hi > lo {
                    ((i128::from(heat.score) - i128::from(lo)) as f64
                        / (i128::from(hi) - i128::from(lo)) as f64) as f32
                } else {
                    1.0
                };
                egui::Color32::from_rgb((60.0 + 195.0 * t) as u8, 80, (255.0 - 195.0 * t) as u8)
            } else {
                REFUSED
            };
            crate::overlays::push_world_circle(
                &mut out,
                view,
                size,
                heat.position,
                Vec3::X * 0.6,
                Vec3::Z * 0.6,
                color,
            );
            label(
                &mut out,
                view,
                size,
                heat.position + Vec3::Y * 0.2,
                format!("score {}", heat.score),
                color,
            );
        }
    }
    out
}
