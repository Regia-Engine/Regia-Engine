// SPDX-License-Identifier: MPL-2.0
//! Audit gap GR-7.6 acceptance (spec 102 § 14): logic graphs drive the match through the
//! `rules::*` tasks and trigger on the match session's events (`match.<kind>`), through
//! `AiPlugin` + `LogicBridgePlugin` + `MatchSessionPlugin`. Fixed steps and counters only.
//!
//! Run: `cargo test -p regia --features logic,net --test logic_rules_nodes`.
#![cfg(all(feature = "logic", feature = "net"))]

use regia::ai::logic::LogicGraphStatus;
use regia::ai::{
    AiPlugin, AiStep, BbValue, Blackboard, BtTaskRegistry, LogicGraph, LogicGraphDef, LogicInbox,
};
use regia::framework::damage::Health;
use regia::framework::player::{PlayerStart, PlayerStartDisabled};
use regia::framework::rules::match_flow::SeatId;
use regia::framework::rules::mode::MatchMode;
use regia::framework::rules::objective::{EndReason, ObjectiveSettings, RulesEvent, ScoreReason};
use regia::framework::rules::session::{MatchSession, SessionSettings};
use regia::framework::rules::teams::SeatRequest;
use regia::framework::rules::{
    LedgerSlot, MatchEvent, MatchEventListener, MatchRules, MatchRulesSet, MatchSessionPlugin,
    ObjectiveEntity, TeamId, default_spawn_point,
};
use regia::logic_bridge::LogicBridgePlugin;
use regia::logic_bridge::rules::{RULES_TASKS, match_key};
use regia::net::NetMode;
use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_ecs::ecs::entity_disabling::Disabled;
use regia_scene::Transform;

const RED_BASE: [f32; 3] = [-20.0, 0.0, 0.0];
const BLUE_BASE: [f32; 3] = [20.0, 0.0, 0.0];
const RED_FLAG: u32 = 900;
const BLUE_FLAG: u32 = 901;

/// A headless app with the AI step, the logic bridge and the match adapter over a
/// capture-the-flag session (capture limit 3) whose two flags sit at their bases.
fn app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins).add_plugins(AiPlugin);
    // Spawn order is PlayerStart arrival, as `PlayerPlugin` tracks it (ADR-8704); without the
    // tracking the start key falls back to position order (ADR-8731, the v0.7 roll-up).
    regia_ecs::spawn_order::try_track_spawn_order::<PlayerStart>(app.world_mut())
        .expect("track PlayerStart arrival");
    let mut session = MatchSession::new(SessionSettings {
        objectives: ObjectiveSettings {
            capture_limit: 3,
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
    app.add_plugins(MatchSessionPlugin).add_plugins(LogicBridgePlugin);
    // The order spec 102 § 14 recommends: a rules task lands in the session step of the same
    // fixed step, so every count below is exact.
    app.configure_sets(FixedUpdate, MatchRulesSet.after(AiStep));
    app
}

/// One frame of one fixed step: the AI step and the session step, then the match-event route.
fn step(app: &mut App) {
    app.world_mut().run_schedule(FixedUpdate);
    app.world_mut().run_schedule(PostUpdate);
}

fn graph(app: &mut App, text: &str) -> LogicGraph {
    let def = LogicGraphDef::from_ron(text).expect("parses");
    LogicGraph::compile(&def, app.world().resource::<BtTaskRegistry>()).expect("compiles")
}

fn spawn_graph(app: &mut App, text: &str) -> Entity {
    let graph = graph(app, text);
    app.world_mut()
        .spawn((graph, Blackboard::new(), LogicInbox::default()))
        .id()
}

/// Seats a player on `team` and spawns its pawn with health at `position`.
fn seated_pawn(app: &mut App, team: u8, position: [f32; 3]) -> (SeatId, Entity) {
    let seats = app
        .world_mut()
        .resource_mut::<MatchRules>()
        .session
        .assign_team(
            &SeatRequest {
                team: Some(team),
                ..SeatRequest::default()
            },
            &[],
        )
        .expect("seat");
    let seat = seats[0];
    let pawn = app
        .world_mut()
        .spawn((
            TeamId(seat.team),
            LedgerSlot(seat.slot),
            Transform::from_translation(Vec3::from(position)),
            Health::new(100.0),
        ))
        .id();
    (seat, pawn)
}

fn move_to(app: &mut App, entity: Entity, position: [f32; 3]) {
    if let Some(mut transform) = app.world_mut().get_mut::<Transform>(entity) {
        transform.translation = Vec3::from(position);
    }
}

fn fires(app: &App, e: Entity, rule: &str) -> u64 {
    app.world()
        .get::<LogicGraphStatus>(e)
        .and_then(|s| s.rules.iter().find(|r| r.name == rule).map(|r| r.fires))
        .expect("rule status")
}

fn trace(app: &App, e: Entity) -> Vec<String> {
    app.world()
        .get::<LogicGraphStatus>(e)
        .map(|s| s.trace.clone())
        .unwrap_or_default()
}

fn failed_actions(app: &App, e: Entity) -> usize {
    app.world()
        .get::<LogicGraph>(e)
        .map(|g| g.trace().filter(|line| line.contains(": action_failed ")).count())
        .unwrap_or(0)
}

/// Writes `key` = `target` (an entity) into `e`'s blackboard: graphs name entities through keys.
fn set_entity(app: &mut App, e: Entity, key: &str, target: Entity) {
    app.world_mut()
        .get_mut::<Blackboard>(e)
        .expect("blackboard")
        .insert(key, BbValue::Entity(target.to_bits()));
}

fn bb<'a>(app: &'a App, e: Entity, key: &str) -> Option<&'a BbValue> {
    app.world().get::<Blackboard>(e).and_then(|b| b.get(key))
}

fn session(app: &App) -> &MatchSession {
    &app.world().resource::<MatchRules>().session
}

#[test]
fn the_bridge_registers_the_five_rules_tasks_as_sim_actions() {
    let app = app();
    let registry = app.world().resource::<BtTaskRegistry>();
    for task in RULES_TASKS {
        assert!(registry.construct(task).is_some(), "{task} registered");
        assert!(!registry.is_presentation(task), "{task} is a sim action");
    }
}

#[test]
fn a_logic_graph_opens_a_gate_on_match_flag_captured() {
    let mut app = app();
    // The gate: a door entity the graph disables ("opens") when team 1 captures.
    let door = app.world_mut().spawn(Transform::default()).id();
    let gate_text = r#"(format_version: 1, locals: [(name: "opened", default: Bool(false))], rules: [
            (name: "open_gate", event: EntityEvent("match.flag_captured"),
             condition: Compare(key: "match_flag_captured_team", op: Eq, value: 1.0),
             actions: [
                (task: "logic::DisableEntity", set: { "target": BbRef("door") }),
                (task: "logic::SetBlackboard", set: { "key": Literal(String("local/opened")), "value": Literal(Bool(true)) })]),
            (name: "taken", event: EntityEvent("match.flag_taken"))])"#;
    let gate = spawn_graph(&mut app, gate_text);
    set_entity(&mut app, gate, "door", door);
    app.world_mut().entity_mut(gate).insert(MatchEventListener);
    // The raider hears the events it is involved in, with its own entity in `match_unit`.
    let (_, red) = seated_pawn(&mut app, 1, RED_BASE);
    let raider_graph = graph(
        &mut app,
        r#"(format_version: 1, rules: [(name: "took", event: EntityEvent("match.flag_taken")),
            (name: "scored", event: EntityEvent("match.flag_captured"))])"#,
    );
    app.world_mut()
        .entity_mut(red)
        .insert((raider_graph, Blackboard::new(), LogicInbox::default()));
    // The flag entity hears its own events too.
    let blue_flag = spawn_graph(
        &mut app,
        r#"(format_version: 1, rules: [(name: "gone", event: EntityEvent("match.flag_taken"))])"#,
    );
    app.world_mut().entity_mut(blue_flag).insert((
        ObjectiveEntity(BLUE_FLAG),
        Transform::from_translation(Vec3::from(BLUE_BASE)),
    ));
    // A bystander graph without the marker hears nothing.
    let bystander = spawn_graph(
        &mut app,
        r#"(format_version: 1, rules: [(name: "any", event: EntityEvent("match.flag_captured"))])"#,
    );

    step(&mut app);
    move_to(&mut app, red, BLUE_BASE);
    step(&mut app);
    move_to(&mut app, red, [0.0, 0.0, 0.0]);
    step(&mut app);
    assert_eq!(fires(&app, gate, "taken"), 1, "{:?}", trace(&app, gate));
    assert_eq!(fires(&app, red, "took"), 1, "{:?}", trace(&app, red));
    assert_eq!(fires(&app, blue_flag, "gone"), 1);
    assert!(app.world().get::<Disabled>(door).is_none(), "no capture yet");

    move_to(&mut app, red, RED_BASE);
    step(&mut app); // the capture: the session step sends it, the route posts it
    step(&mut app); // the graphs take it
    assert_eq!(session(&app).board.team_scores[1], 1);
    assert_eq!(fires(&app, gate, "open_gate"), 1, "{:?}", trace(&app, gate));
    assert!(app.world().get::<Disabled>(door).is_some(), "the gate is open");
    assert_eq!(
        app.world()
            .get::<LogicGraph>(gate)
            .and_then(|g| g.locals().get("opened").cloned()),
        Some(BbValue::Bool(true))
    );
    assert_eq!(
        bb(&app, gate, &match_key("flag_captured", "team")),
        Some(&BbValue::Int(1))
    );
    assert_eq!(
        bb(&app, gate, &match_key("flag_captured", "score")),
        Some(&BbValue::Int(1))
    );
    assert_eq!(
        bb(&app, gate, &match_key("flag_captured", "objective")),
        Some(&BbValue::Int(i64::from(BLUE_FLAG)))
    );
    assert_eq!(
        bb(&app, gate, &match_key("flag_captured", "unit")),
        Some(&BbValue::Entity(red.to_bits()))
    );
    // The capture credited the carrier's seat in the same step: the gate also heard
    // `match.player_scored`, under its own keys, and the capture's keys are untouched.
    assert_eq!(
        bb(&app, gate, &match_key("player_scored", "reason")),
        Some(&BbValue::String("objective".to_string()))
    );
    assert_eq!(fires(&app, red, "scored"), 1);
    assert_eq!(fires(&app, bystander, "any"), 0, "no marker, not involved");
}

#[test]
fn a_capture_by_the_other_team_leaves_the_gate_shut() {
    let mut app = app();
    let door = app.world_mut().spawn(Transform::default()).id();
    let gate_text = r#"(format_version: 1, rules: [
            (name: "open_gate", event: EntityEvent("match.flag_captured"),
             condition: Compare(key: "match_flag_captured_team", op: Eq, value: 1.0),
             actions: [(task: "logic::DisableEntity", set: { "target": BbRef("door") })])])"#;
    let gate = spawn_graph(&mut app, gate_text);
    set_entity(&mut app, gate, "door", door);
    app.world_mut().entity_mut(gate).insert(MatchEventListener);
    let (_, blue) = seated_pawn(&mut app, 2, BLUE_BASE);
    step(&mut app);
    move_to(&mut app, blue, RED_BASE);
    step(&mut app);
    move_to(&mut app, blue, BLUE_BASE);
    step(&mut app);
    step(&mut app);
    assert_eq!(session(&app).board.team_scores[2], 1);
    assert_eq!(fires(&app, gate, "open_gate"), 0, "the condition held the rule");
    assert!(app.world().get::<Disabled>(door).is_none());
}

#[test]
fn add_score_task_scores_on_the_host_and_is_refused_on_a_client() {
    let text = r#"(format_version: 1, rules: [
        (name: "team", event: EntityEvent("bonus"), actions: [
            (task: "rules::AddScore", set: { "team": Literal(Int(2)), "points": Literal(Int(5)) })]),
        (name: "seat", event: EntityEvent("seat_bonus"), actions: [
            (task: "rules::AddScore", set: { "team": Literal(Int(1)), "seat": Literal(Int(0)), "points": Literal(Int(3)) })]),
        (name: "mine", event: EntityEvent("my_bonus"), actions: [
            (task: "rules::AddScore", set: { "points": Literal(Int(2)) })])])"#;
    // `mine` names no team: it fails on every peer (a missing parameter).
    for mode in [NetMode::Offline, NetMode::ListenServer, NetMode::Client] {
        let mut app = app();
        app.insert_resource(mode);
        let (seat, _) = seated_pawn(&mut app, 1, RED_BASE);
        let g = spawn_graph(&mut app, text);
        step(&mut app);
        for event in ["bonus", "seat_bonus", "my_bonus"] {
            app.world_mut()
                .get_mut::<LogicInbox>(g)
                .expect("inbox")
                .push(event);
        }
        step(&mut app);
        step(&mut app);
        let s = session(&app);
        let row = s.ledgers.row(seat).expect("row");
        if mode.is_client() {
            assert_eq!(s.board.team_scores[2], 0, "a client changes no score");
            assert_eq!(row.score, 0);
            assert_eq!(failed_actions(&app, g), 3, "refused, and the trace says so");
        } else {
            assert_eq!(s.board.team_scores[2], 5, "{mode:?}: the team score");
            assert_eq!(row.score, 3, "{mode:?}: the seat score");
            assert_eq!(failed_actions(&app, g), 1, "only `mine` fails");
        }
    }
}

#[test]
fn add_score_on_a_seat_entity_sends_match_player_scored_back_to_its_pawn() {
    let mut app = app();
    let (seat, pawn) = seated_pawn(&mut app, 1, RED_BASE);
    let pawn_graph = graph(
        &mut app,
        r#"(format_version: 1, rules: [
            (name: "bonus", event: EntityEvent("bonus"), actions: [
                (task: "rules::AddScore", set: { "entity": BbRef("self"), "points": Literal(Int(4)) })]),
            (name: "scored", event: EntityEvent("match.player_scored"))])"#,
    );
    let mut board = Blackboard::new();
    board.insert("self", BbValue::Entity(pawn.to_bits()));
    app.world_mut()
        .entity_mut(pawn)
        .insert((pawn_graph, board, LogicInbox::default()));
    step(&mut app);
    app.world_mut()
        .get_mut::<LogicInbox>(pawn)
        .expect("inbox")
        .push("bonus");
    step(&mut app); // the task scores and queues the event; the session step sends it
    step(&mut app); // the pawn's graph takes `match.player_scored`
    assert_eq!(session(&app).ledgers.row(seat).map(|r| r.score), Some(4));
    assert_eq!(fires(&app, pawn, "scored"), 1, "{:?}", trace(&app, pawn));
    assert_eq!(
        bb(&app, pawn, &match_key("player_scored", "score")),
        Some(&BbValue::Int(4))
    );
    assert_eq!(
        bb(&app, pawn, &match_key("player_scored", "reason")),
        Some(&BbValue::String("scripted".to_string()))
    );
}

#[test]
fn end_match_task_ends_the_match_with_its_reason() {
    let mut app = app();
    let text = r#"(format_version: 1, rules: [
        (name: "forfeit", event: EntityEvent("give_up"), actions: [
            (task: "rules::EndMatch", set: { "team": Literal(Int(2)), "reason": Literal(String("forfeit")) })]),
        (name: "bad", event: EntityEvent("bad"), actions: [
            (task: "rules::EndMatch", set: { "reason": Literal(String("bored")) })]),
        (name: "over", event: EntityEvent("match.match_over"))])"#;
    let g = spawn_graph(&mut app, text);
    app.world_mut().entity_mut(g).insert(MatchEventListener);
    step(&mut app);
    // An unknown reason fails the action and changes nothing.
    app.world_mut()
        .get_mut::<LogicInbox>(g)
        .expect("inbox")
        .push("bad");
    step(&mut app);
    assert!(!session(&app).board.match_over);
    assert_eq!(failed_actions(&app, g), 1);

    app.world_mut()
        .get_mut::<LogicInbox>(g)
        .expect("inbox")
        .push("give_up");
    step(&mut app);
    let board = &session(&app).board;
    assert!(board.match_over);
    assert_eq!(board.end_reason, Some(EndReason::Forfeit));
    step(&mut app);
    assert_eq!(fires(&app, g, "over"), 1, "{:?}", trace(&app, g));
    assert_eq!(
        bb(&app, g, &match_key("match_over", "team")),
        Some(&BbValue::Int(2))
    );
    assert_eq!(
        bb(&app, g, &match_key("match_over", "reason")),
        Some(&BbValue::String("forfeit".to_string()))
    );
    // A second end is refused: the first reason stands.
    app.world_mut()
        .get_mut::<LogicInbox>(g)
        .expect("inbox")
        .push("give_up");
    step(&mut app);
    assert_eq!(session(&app).board.end_reason, Some(EndReason::Forfeit));
    assert_eq!(failed_actions(&app, g), 2);
}

#[test]
fn set_spawn_enabled_task_removes_a_point_from_selection() {
    let mut app = app();
    let north = app
        .world_mut()
        .spawn((
            PlayerStart,
            Transform::from_translation(Vec3::new(0.0, 0.0, 50.0)),
        ))
        .id();
    let _south = app
        .world_mut()
        .spawn((
            PlayerStart,
            Transform::from_translation(Vec3::new(0.0, 0.0, -50.0)),
        ))
        .id();
    let not_a_start = app.world_mut().spawn(Transform::default()).id();
    let text = r#"(format_version: 1, rules: [
            (name: "close", event: EntityEvent("close"), actions: [
                (task: "rules::SetSpawnEnabled", set: { "entity": BbRef("north"), "on": Literal(Bool(false)) })]),
            (name: "reopen", event: EntityEvent("reopen"), actions: [
                (task: "rules::SetSpawnEnabled", set: { "entity": BbRef("north") })]),
            (name: "wrong", event: EntityEvent("wrong"), actions: [
                (task: "rules::SetSpawnEnabled", set: { "entity": BbRef("other"), "on": Literal(Bool(false)) })])])"#;
    let g = spawn_graph(&mut app, text);
    set_entity(&mut app, g, "north", north);
    set_entity(&mut app, g, "other", not_a_start);
    let player = app.world_mut().spawn_empty().id();
    step(&mut app);
    assert_eq!(
        default_spawn_point(app.world_mut(), player),
        Vec3::new(0.0, 0.0, 50.0)
    );

    app.world_mut()
        .get_mut::<LogicInbox>(g)
        .expect("inbox")
        .push("close");
    step(&mut app);
    assert!(app.world().get::<PlayerStartDisabled>(north).is_some());
    assert_eq!(
        default_spawn_point(app.world_mut(), player),
        Vec3::new(0.0, 0.0, -50.0),
        "the closed point is out of selection"
    );

    app.world_mut()
        .get_mut::<LogicInbox>(g)
        .expect("inbox")
        .push("wrong");
    step(&mut app);
    assert_eq!(failed_actions(&app, g), 1, "only a PlayerStart can be switched");
    assert!(app.world().get::<PlayerStartDisabled>(not_a_start).is_none());

    app.world_mut()
        .get_mut::<LogicInbox>(g)
        .expect("inbox")
        .push("reopen");
    step(&mut app);
    assert_eq!(
        default_spawn_point(app.world_mut(), player),
        Vec3::new(0.0, 0.0, 50.0)
    );
}

#[test]
fn every_start_disabled_falls_back_to_the_first_start() {
    let mut app = app();
    let a = app
        .world_mut()
        .spawn((
            PlayerStart,
            PlayerStartDisabled,
            Transform::from_translation(Vec3::X),
        ))
        .id();
    app.world_mut().spawn((
        PlayerStart,
        PlayerStartDisabled,
        Transform::from_translation(Vec3::Y),
    ));
    let player = app.world_mut().spawn_empty().id();
    assert!(app.world().get::<PlayerStartDisabled>(a).is_some());
    assert_eq!(default_spawn_point(app.world_mut(), player), Vec3::X);
}

#[test]
fn lock_objective_task_holds_the_flag_until_unlocked() {
    let mut app = app();
    let text = format!(
        r#"(format_version: 1, rules: [
            (name: "lock", event: EntityEvent("lock"), actions: [
                (task: "rules::LockObjective", set: {{ "id": Literal(Int({BLUE_FLAG})) }})]),
            (name: "unlock", event: EntityEvent("unlock"), actions: [
                (task: "rules::LockObjective", set: {{ "id": Literal(Int({BLUE_FLAG})), "on": Literal(Bool(false)) }})]),
            (name: "missing", event: EntityEvent("missing"), actions: [
                (task: "rules::LockObjective", set: {{ "id": Literal(Int(4242)) }})])])"#
    );
    let g = spawn_graph(&mut app, &text);
    let (_, red) = seated_pawn(&mut app, 1, RED_BASE);
    step(&mut app);
    app.world_mut()
        .get_mut::<LogicInbox>(g)
        .expect("inbox")
        .push("lock");
    app.world_mut()
        .get_mut::<LogicInbox>(g)
        .expect("inbox")
        .push("missing");
    step(&mut app);
    assert!(session(&app).board.is_locked(BLUE_FLAG));
    assert_eq!(failed_actions(&app, g), 1, "an unknown objective fails");
    move_to(&mut app, red, BLUE_BASE);
    step(&mut app);
    assert_eq!(
        session(&app)
            .board
            .objective(BLUE_FLAG)
            .and_then(|f| f.carrier_id),
        None,
        "the locked flag stays at its base"
    );
    app.world_mut()
        .get_mut::<LogicInbox>(g)
        .expect("inbox")
        .push("unlock");
    step(&mut app);
    step(&mut app);
    assert_eq!(
        session(&app)
            .board
            .objective(BLUE_FLAG)
            .and_then(|f| f.carrier_id),
        Some(red.index().index())
    );
}

#[test]
fn set_team_task_moves_an_unseated_entity_and_follows_the_switch_policy_for_a_seat() {
    let mut app = app();
    let (seat, pawn) = seated_pawn(&mut app, 1, RED_BASE);
    let bot = app.world_mut().spawn(TeamId(1)).id();
    let text = r#"(format_version: 1, rules: [
            (name: "bot", event: EntityEvent("bot"), actions: [
                (task: "rules::SetTeam", set: { "entity": BbRef("bot"), "team": Literal(Int(2)) })]),
            (name: "pawn", event: EntityEvent("pawn"), actions: [
                (task: "rules::SetTeam", set: { "entity": BbRef("pawn"), "team": Literal(Int(2)) })])])"#;
    let g = spawn_graph(&mut app, text);
    set_entity(&mut app, g, "bot", bot);
    set_entity(&mut app, g, "pawn", pawn);
    step(&mut app);
    app.world_mut()
        .get_mut::<LogicInbox>(g)
        .expect("inbox")
        .push("bot");
    // The default switch policy is `never`: the seated pawn stays.
    app.world_mut()
        .get_mut::<LogicInbox>(g)
        .expect("inbox")
        .push("pawn");
    step(&mut app);
    assert_eq!(app.world().get::<TeamId>(bot), Some(&TeamId(2)));
    assert_eq!(app.world().get::<TeamId>(pawn), Some(&TeamId(1)));
    assert_eq!(failed_actions(&app, g), 1, "match_team_switch_refused");
    assert!(session(&app).flow.seated.contains(&seat));

    // With switches allowed between matches, a switch while the flow is between matches moves the
    // seat, and the pawn's tags follow it.
    {
        let mut rules = app.world_mut().resource_mut::<MatchRules>();
        rules.session.teams.switch = regia::framework::rules::teams::TeamSwitch::BetweenMatches;
    }
    let between = session(&app).flow.between;
    app.world_mut()
        .get_mut::<LogicInbox>(g)
        .expect("inbox")
        .push("pawn");
    step(&mut app);
    if between {
        assert_eq!(app.world().get::<TeamId>(pawn), Some(&TeamId(2)));
        assert_eq!(
            app.world().get::<LedgerSlot>(pawn).map(|s| s.0),
            session(&app)
                .flow
                .seated
                .iter()
                .find(|s| s.team == 2)
                .map(|s| s.slot)
        );
    } else {
        assert_eq!(app.world().get::<TeamId>(pawn), Some(&TeamId(1)));
    }
}

#[test]
fn match_events_carry_the_kind_tag_the_protocol_reports() {
    let event = MatchEvent(RulesEvent::PlayerScored {
        tick: 3,
        team: 1,
        slot: 0,
        reason: ScoreReason::Scripted,
        score: 2,
    });
    assert_eq!(event.logic_name(), "match.player_scored");
}

/// The route resolves `unit` by entity index only, in PostUpdate. Bevy reuses an index after the
/// local free buffer flushes (128 frees). A pawn that took a flag, then died, must not hand
/// `match.flag_taken` to whatever entity is born with that index.
#[test]
fn a_recycled_contestant_index_does_not_inherit_the_departed_units_event() {
    let mut app = app();
    let (_, pawn) = seated_pawn(&mut app, 1, RED_BASE);
    let index = pawn.index().index();
    app.world_mut().write_message(MatchEvent(RulesEvent::FlagTaken {
        tick: 1,
        flag: BLUE_FLAG,
        unit: index,
        team: 1,
        flag_team: 2,
    }));
    assert!(app.world_mut().despawn(pawn), "the taker despawns");
    // Fill the allocator's 128-entry local free buffer so the pawn's index is flushed back to
    // the shared free list (the 128th later free flushes the buffer the pawn already sits in).
    let mut fillers = Vec::new();
    for _ in 0..128 {
        fillers.push(app.world_mut().spawn_empty().id());
    }
    for filler in fillers {
        assert!(app.world_mut().despawn(filler));
    }
    let mut recycled = None;
    for _ in 0..128 {
        let entity = app.world_mut().spawn(TeamId(1)).id();
        if entity.index().index() == index {
            recycled = Some(entity);
            break;
        }
    }
    let Some(recycled) = recycled else {
        panic!("contestant index {index} was not reused within 128 spawns after the free-list flush");
    };
    let graph = graph(
        &mut app,
        r#"(format_version: 1, rules: [(name: "took", event: EntityEvent("match.flag_taken"))])"#,
    );
    app.world_mut()
        .entity_mut(recycled)
        .insert((graph, Blackboard::new(), LogicInbox::default()));
    app.world_mut().run_schedule(PostUpdate);
    let inbox = app
        .world()
        .get::<LogicInbox>(recycled)
        .map(|inbox| inbox.events.clone())
        .unwrap_or_default();
    assert!(
        !inbox.iter().any(|name| name == "match.flag_taken"),
        "contestant index {index} was reused by {recycled:?}; inbox {inbox:?}"
    );
}
