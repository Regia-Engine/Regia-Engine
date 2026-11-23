// SPDX-License-Identifier: MPL-2.0
//! Replicated match state end to end (`book/agents/match-rules.md`, "Replicated match state"): a
//! dedicated server with `MatchSessionPlugin` and `MatchStateReplicationPlugin` steps a
//! capture-the-flag match; client apps with only `MatchStateReplicationPlugin` read the views and
//! the `MatchEvent` messages. Datagrams are moved by hand, one frame per fixed tick; nothing
//! sleeps and nothing reads the wall clock.

use regia_app::enable_fixed_step;
use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_framework::damage::Health;
use regia_framework::rules::match_flow::FlowInput;
use regia_framework::rules::match_replication::{
    MATCH_EVENT_FORMAT, ObjectiveViewState, encode_match_event, objective_position, objective_views,
    team_scores_view,
};
use regia_framework::rules::mode::MatchMode;
use regia_framework::rules::objective::{ObjectiveSettings, RulesEvent};
use regia_framework::rules::phases::{MatchPhase, PhaseSettings};
use regia_framework::rules::session::{MatchSession, SessionSettings};
use regia_framework::rules::{
    GameRules, LedgerSlot, MatchEvent, MatchEventMsg, MatchPhaseView, MatchRules, MatchSessionPlugin,
    MatchStateCounters, MatchStateEntity, MatchStateReplicationPlugin, ObjectiveViews, PlayerScoreView,
    ReplicatedMatchRules, RulesCtx, TeamId, TeamScoresView, match_session_score_sheet,
};
use regia_net::model::{LocalPeer, NetMode, PeerId};
use regia_net::runtime::{NetNode, server_add_peer};
use regia_net::{NetPlugin, NetSender, Replicated, TransformReplication};
use regia_scene::Transform;

const HZ: f64 = 60.0;
const RED_BASE: [f32; 3] = [-20.0, 0.0, 0.0];
const BLUE_BASE: [f32; 3] = [20.0, 0.0, 0.0];
const RED_FLAG: u32 = 900;
const BLUE_FLAG: u32 = 901;

/// Every `MatchEvent` an app saw, in order.
#[derive(Resource, Default)]
struct EventLog(Vec<RulesEvent>);

fn log_events(mut reader: MessageReader<MatchEvent>, mut log: ResMut<EventLog>) {
    log.0.extend(reader.read().map(|event| event.0.clone()));
}

fn server_app(capture_limit: u32, time_limit: Option<u64>) -> App {
    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins,
        NetPlugin {
            mode: NetMode::DedicatedServer,
        },
    ));
    enable_fixed_step(&mut app, HZ);
    app.insert_resource(LocalPeer(Some(PeerId::SERVER)));
    let mut session = MatchSession::new(SessionSettings {
        objectives: ObjectiveSettings {
            capture_limit,
            ..ObjectiveSettings::default()
        },
        ..SessionSettings::default()
    });
    session.flow.settings.phases = PhaseSettings {
        time_limit_ticks: time_limit,
        ..PhaseSettings::default()
    };
    session.board.reset(MatchMode::CaptureTheFlag);
    session.board.spawn_flag(RED_FLAG, 1, RED_BASE).expect("red flag");
    session
        .board
        .spawn_flag(BLUE_FLAG, 2, BLUE_BASE)
        .expect("blue flag");
    session
        .apply(&FlowInput::Seat { team: 1, slot: 0 })
        .expect("red seat");
    session
        .apply(&FlowInput::Seat { team: 2, slot: 0 })
        .expect("blue seat");
    app.insert_resource(MatchRules { session });
    app.add_plugins((MatchSessionPlugin, MatchStateReplicationPlugin));
    app.init_resource::<EventLog>();
    app.add_systems(Update, log_events);
    app
}

fn client_app(peer: PeerId) -> App {
    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins,
        NetPlugin {
            mode: NetMode::Client,
        },
        MatchStateReplicationPlugin,
    ));
    enable_fixed_step(&mut app, HZ);
    app.insert_resource(LocalPeer(Some(peer)));
    app.init_resource::<EventLog>();
    app.add_systems(Update, log_events);
    app
}

fn pawn(app: &mut App, team: u8, at: [f32; 3]) -> Entity {
    app.world_mut()
        .spawn((
            Replicated {
                transform: TransformReplication::PositionOnly,
                ..Replicated::default()
            },
            Transform::from_translation(Vec3::from(at)),
            TeamId(team),
            LedgerSlot(0),
            Health::new(100.0),
        ))
        .id()
}

fn move_to(app: &mut App, entity: Entity, at: [f32; 3]) {
    if let Some(mut t) = app.world_mut().get_mut::<Transform>(entity) {
        t.translation = Vec3::from(at);
    }
}

fn set_health(app: &mut App, entity: Entity, current: f32) {
    if let Some(mut h) = app.world_mut().get_mut::<Health>(entity) {
        h.current = current;
    }
}

/// One frame on every joined app; datagrams cross at once. `clients[i]` is peer `i + 1`.
fn pump(server: &mut App, clients: &mut [App], joined: &[bool]) {
    server.update();
    let down = std::mem::take(&mut server.world_mut().resource_mut::<NetNode>().outbox);
    for mut d in down {
        let index = d.peer.0 as usize;
        if (1..=clients.len()).contains(&index) && joined[index - 1] {
            d.peer = PeerId::SERVER;
            clients[index - 1]
                .world_mut()
                .resource_mut::<NetNode>()
                .inbox
                .push(d);
        }
    }
    for (i, client) in clients.iter_mut().enumerate() {
        if !joined[i] {
            continue;
        }
        client.update();
        let up = std::mem::take(&mut client.world_mut().resource_mut::<NetNode>().outbox);
        let mut node = server.world_mut().resource_mut::<NetNode>();
        node.inbox.extend(up.into_iter().map(|mut d| {
            d.peer = PeerId(i as u32 + 1);
            d
        }));
    }
}

fn views(app: &mut App) -> Option<(MatchPhaseView, TeamScoresView, ObjectiveViews)> {
    regia_framework::rules::match_replication::match_views(app.world_mut())
}

fn player_views(app: &mut App) -> Vec<PlayerScoreView> {
    let mut q = app.world_mut().query::<&PlayerScoreView>();
    let mut out: Vec<PlayerScoreView> = q.iter(app.world()).cloned().collect();
    out.sort_by_key(|v| (v.team, v.slot));
    out
}

fn server_board(app: &App) -> TeamScoresView {
    team_scores_view(&app.world().resource::<MatchRules>().session)
}

fn server_objectives(app: &App) -> ObjectiveViews {
    objective_views(&app.world().resource::<MatchRules>().session)
}

/// The scripted raid: red takes the blue flag, dies on the way home (the flag drops), revives on
/// the spot (takes it again from the ground) and captures. `frame` is the pump index.
fn raid_step(server: &mut App, red: Entity, frame: u32) {
    match frame {
        10 => move_to(server, red, BLUE_BASE),
        14 => move_to(server, red, [0.0, 0.0, 0.0]),
        18 => set_health(server, red, 0.0),
        22 => set_health(server, red, 100.0),
        26 => move_to(server, red, RED_BASE),
        _ => {}
    }
}

#[test]
fn a_client_sees_the_servers_phase_clock_and_team_scores() {
    let mut server = server_app(3, Some(6000));
    let red = pawn(&mut server, 1, RED_BASE);
    let _blue = pawn(&mut server, 2, [0.0, 0.0, 30.0]);
    let mut clients = vec![client_app(PeerId(1))];
    server_add_peer(server.world_mut(), PeerId(1));
    let joined = [true];
    // The server's views after each frame; the client may trail by one snapshot.
    let mut history: Vec<(TeamScoresView, MatchPhaseView)> = Vec::new();
    let mut compared = 0u32;
    for frame in 0..60 {
        raid_step(&mut server, red, frame);
        pump(&mut server, &mut clients, &joined);
        let (server_phase, server_teams, _) = views(&mut server).expect("the server spawned the match state");
        assert_eq!(
            server_teams,
            server_board(&server),
            "frame {frame}: the server view is its board"
        );
        history.push((server_teams, server_phase));
        let Some((phase, teams, _)) = views(&mut clients[0]) else {
            assert!(frame < 3, "frame {frame}: no match state on the client");
            continue;
        };
        let n = history.len();
        let within_one = history[n.saturating_sub(2)..]
            .iter()
            .any(|(t, p)| *t == teams && *p == phase);
        assert!(
            within_one,
            "frame {frame}: client teams {teams:?} phase {phase:?} are not the server's last two {:?}",
            &history[n.saturating_sub(2)..]
        );
        compared += 1;
    }
    assert!(compared > 50, "the client compared {compared} frames");
    let (phase, teams, objectives) = views(&mut clients[0]).expect("client views");
    assert_eq!(teams.score(1), 1, "the capture reached the client");
    assert_eq!(phase.phase(), MatchPhase::Live);
    assert_eq!(phase.time_left_ticks, Some(6000 - phase.match_tick));
    assert!(!phase.match_over);
    // The client's clock read at its newest server tick is the server's clock.
    let client_tick = clients[0].world().resource::<NetNode>().tick.0;
    let session_tick = server.world().resource::<MatchRules>().session.flow.match_tick;
    let client_clock = phase.match_tick_at(client_tick);
    assert!(
        client_clock <= session_tick && session_tick - client_clock <= 2,
        "client clock {client_clock} trails the server's {session_tick} by at most one snapshot"
    );
    assert_eq!(objectives.items.len(), 2);
    assert_eq!(objectives, server_objectives(&server));
    // The client's rules answer the scoreboard from the views: the server's sheet, row for row.
    let world = clients[0].world_mut();
    let sheet = ReplicatedMatchRules
        .scoreboard(&mut RulesCtx::new(world))
        .expect("a sheet from the views");
    let expected = match_session_score_sheet(&server.world().resource::<MatchRules>().session);
    assert_eq!(sheet, expected);
    assert!(!ReplicatedMatchRules.is_match_over(clients[0].world()));
    let players = player_views(&mut clients[0]);
    assert_eq!(players.len(), 2);
    assert_eq!(players[0].objective_points, 1, "red's ledger has the capture");
}

#[test]
fn a_late_joiner_gets_the_current_scores_and_objective_states() {
    let mut server = server_app(3, None);
    let red = pawn(&mut server, 1, RED_BASE);
    let _blue = pawn(&mut server, 2, [0.0, 0.0, 30.0]);
    let mut clients = vec![client_app(PeerId(1)), client_app(PeerId(2))];
    server_add_peer(server.world_mut(), PeerId(1));
    let mut joined = [true, false];
    // The raid runs to its capture; then red takes the blue flag again and holds it.
    for frame in 0..40 {
        raid_step(&mut server, red, frame);
        if frame == 32 {
            move_to(&mut server, red, BLUE_BASE);
        }
        if frame == 36 {
            move_to(&mut server, red, [5.0, 0.0, 5.0]);
        }
        pump(&mut server, &mut clients, &joined);
    }
    assert_eq!(server_board(&server).score(1), 1);
    let carried = server_objectives(&server);
    assert!(
        carried
            .items
            .iter()
            .any(|o| matches!(o.state, ObjectiveViewState::Carried { .. })),
        "{carried:?}"
    );
    server_add_peer(server.world_mut(), PeerId(2));
    joined[1] = true;
    let mut frames_to_views = None;
    for frame in 0..10 {
        pump(&mut server, &mut clients, &joined);
        if let Some((_, teams, objectives)) = views(&mut clients[1]) {
            frames_to_views = Some(frame);
            assert_eq!(teams, server_board(&server));
            assert_eq!(objectives, server_objectives(&server));
            assert_eq!(player_views(&mut clients[1]), player_views(&mut server));
            // The carried flag's view names the carrier's seat; the client finds it at the
            // carrier's replicated position.
            let flag = objectives
                .items
                .iter()
                .find(|o| matches!(o.state, ObjectiveViewState::Carried { .. }))
                .cloned()
                .expect("a carried flag");
            assert!(
                matches!(flag.state, ObjectiveViewState::Carried { seat: Some(seat), .. } if seat.team == 1 && seat.slot == 0),
                "{flag:?}"
            );
            assert_eq!(
                flag.position,
                Vec3::from(BLUE_BASE),
                "a carried flag's view keeps its base"
            );
            break;
        }
    }
    assert!(
        frames_to_views.is_some(),
        "the late joiner got no views in 10 frames"
    );
    // A few more frames for the carrier's transform; the flag is where red stands.
    for _ in 0..5 {
        pump(&mut server, &mut clients, &joined);
    }
    let (_, _, objectives) = views(&mut clients[1]).expect("views");
    let flag = objectives
        .items
        .iter()
        .find(|o| matches!(o.state, ObjectiveViewState::Carried { .. }))
        .cloned()
        .expect("still carried");
    let red_at = server
        .world()
        .get::<Transform>(red)
        .map(|t| t.translation)
        .expect("red");
    let shown = objective_position(clients[1].world_mut(), &flag);
    assert!(
        shown.distance(red_at) < 0.01,
        "flag shown at {shown}, red at {red_at}"
    );
    // Events from before the join are not replayed.
    let late_log = &clients[1].world().resource::<EventLog>().0;
    assert!(
        !late_log
            .iter()
            .any(|e| matches!(e, RulesEvent::FlagCaptured { .. })),
        "{late_log:?}"
    );
}

#[test]
fn match_events_reach_the_client_in_order_once() {
    let mut server = server_app(1, None);
    let red = pawn(&mut server, 1, RED_BASE);
    let _blue = pawn(&mut server, 2, [0.0, 0.0, 30.0]);
    let mut clients = vec![client_app(PeerId(1))];
    server_add_peer(server.world_mut(), PeerId(1));
    let joined = [true];
    for frame in 0..50 {
        raid_step(&mut server, red, frame);
        pump(&mut server, &mut clients, &joined);
    }
    let server_log = server.world().resource::<EventLog>().0.clone();
    let client_log = clients[0].world().resource::<EventLog>().0.clone();
    assert_eq!(
        client_log, server_log,
        "the client saw the server's events, in order, once each"
    );
    let kinds: Vec<&str> = client_log
        .iter()
        .filter_map(|e| match e {
            RulesEvent::FlagTaken { .. } => Some("taken"),
            RulesEvent::FlagDropped { .. } => Some("dropped"),
            RulesEvent::FlagCaptured { .. } => Some("captured"),
            RulesEvent::MatchOver { .. } => Some("over"),
            _ => None,
        })
        .collect();
    assert_eq!(kinds, ["taken", "dropped", "taken", "captured", "over"]);
    let counters = *clients[0].world().resource::<MatchStateCounters>();
    assert_eq!(counters.events_received, client_log.len() as u64);
    assert_eq!(counters.events_malformed + counters.events_oversized, 0);
    let sent = server.world().resource::<MatchStateCounters>().events_sent;
    assert_eq!(sent, server_log.len() as u64);
    let (phase, _, _) = views(&mut clients[0]).expect("views");
    assert!(phase.match_over);
    assert_eq!(phase.winner, 1);
    assert_eq!(phase.end_reason(), "capture_limit_reached");
    assert!(ReplicatedMatchRules.is_match_over(clients[0].world()));
}

/// Server: sends the queued raw payloads as `MatchEventMsg`s (a hostile or broken server).
#[derive(Resource, Default)]
struct RawPayloads(Vec<Vec<u8>>);

fn send_raw(mut raw: ResMut<RawPayloads>, mut sender: NetSender) {
    for payload in raw.0.drain(..) {
        let _ = sender.broadcast_session(MatchEventMsg { payload });
    }
}

#[test]
fn oversized_or_unknown_match_event_messages_are_dropped_with_a_counter() {
    let mut server = server_app(3, None);
    server
        .init_resource::<RawPayloads>()
        .add_systems(FixedUpdate, send_raw);
    let mut clients = vec![client_app(PeerId(1))];
    server_add_peer(server.world_mut(), PeerId(1));
    let joined = [true];
    for _ in 0..5 {
        pump(&mut server, &mut clients, &joined);
    }
    let good = encode_match_event(&RulesEvent::Resumed { tick: 7 }).expect("encodes");
    let mut payloads = vec![
        vec![0u8; 300],                     // longer than the default 256-byte cap
        vec![MATCH_EVENT_FORMAT, 250],      // unknown kind
        vec![MATCH_EVENT_FORMAT + 1, 8, 7], // unknown format
        Vec::new(),                         // empty
        {
            let mut trailing = good.clone();
            trailing.extend([1, 2, 3]);
            trailing
        },
    ];
    // Seeded garbage: decoded or dropped, never a panic.
    let mut state = 0xD1B5_4A32_D192_ED03u64;
    for _ in 0..200 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let len = 1 + (state % 40) as usize;
        payloads.push(
            (0..len)
                .map(|i| (state.rotate_left(i as u32 * 5) & 0xff) as u8)
                .collect(),
        );
    }
    payloads.push(good);
    let sent = payloads.len() as u64;
    server.world_mut().resource_mut::<RawPayloads>().0 = payloads;
    for _ in 0..5 {
        pump(&mut server, &mut clients, &joined);
    }
    let counters = *clients[0].world().resource::<MatchStateCounters>();
    assert_eq!(counters.events_oversized, 1, "{counters:?}");
    assert!(counters.events_malformed >= 4, "{counters:?}");
    assert_eq!(
        counters.events_received + counters.events_malformed + counters.events_oversized,
        sent,
        "every payload is counted once: {counters:?}"
    );
    let log = &clients[0].world().resource::<EventLog>().0;
    assert_eq!(log.len() as u64, counters.events_received);
    assert_eq!(log.last(), Some(&RulesEvent::Resumed { tick: 7 }));
    // The client still reads the match after the garbage.
    assert!(
        clients[0]
            .world_mut()
            .query_filtered::<(), With<MatchStateEntity>>()
            .iter(clients[0].world())
            .next()
            .is_some()
    );
}
