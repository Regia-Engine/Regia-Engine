// SPDX-License-Identifier: MIT-0
//! The template keeps its tally in the engine's match session and shows it through the engine's
//! replicated views (`book/agents/match-rules.md`, "Replicated match state"): no game-side score
//! component travels; every avatar carries a `PlayerScoreView` on the server and on each client,
//! and the clients hold the match-state entity.
//!
//! Time is the links' manual clock (one app update = one tick); nothing sleeps.

use std::net::SocketAddr;

use net_game::{Avatar, Bot, NetGameWire, PadSource, ShotLog, Side, avatars, headless_app, link_options};
use regia::app::App;
use regia::core::math::Vec3;
use regia::framework::player::{PlayerStart, SpawnPoint};
use regia::framework::rules::match_replication::team_scores_view;
use regia::framework::rules::{MatchRules, MatchStateEntity, PlayerScoreView, TeamId, TeamScoresView};
use regia::net::link::{LinkClock, LinkOptions, NetLink};
use regia::net::replication::ReplicationRegistry;
use regia::net::{ConditionerConfig, LoopbackNet};
use regia::prelude::*;
use regia_scene::transform::Transform;

fn manual() -> LinkClock {
    LinkClock::Manual {
        start_ms: 0,
        step_us: 16_667,
    }
}

fn score_views(app: &mut App) -> Vec<PlayerScoreView> {
    let mut q = app.world_mut().query::<&PlayerScoreView>();
    let mut out: Vec<PlayerScoreView> = q.iter(app.world()).cloned().collect();
    out.sort_by_key(|v| (v.team, v.slot));
    out
}

#[test]
fn net_game_sample_uses_the_engine_score_views() {
    let net = LoopbackNet::new();
    let mut server = headless_app(Side::Server);
    let opts = link_options(manual(), ConditionerConfig::default());
    let addr = SocketAddr::from(([127, 0, 0, 1], 7777));
    let socket = net.bind(addr).expect("bind");
    server
        .world_mut()
        .resource_mut::<NetLink>()
        .host::<NetGameWire>(Box::new(socket), &opts)
        .expect("host");
    let key = server.world().resource::<NetLink>().server_key();
    let mut clients = Vec::new();
    for i in 0..2u64 {
        let mut app = headless_app(Side::Client);
        let opts = LinkOptions {
            server_key: key,
            name: format!("bot{i}"),
            ..link_options(manual(), ConditionerConfig::default())
        };
        let socket = net.bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("bind");
        app.world_mut()
            .resource_mut::<NetLink>()
            .connect::<NetGameWire>(Box::new(socket), addr, &opts)
            .expect("connect");
        clients.push(app);
    }
    let step = |server: &mut App, clients: &mut [App]| {
        server.update();
        for c in clients.iter_mut() {
            c.update();
        }
    };
    let mut joined = false;
    for _ in 0..600 {
        step(&mut server, &mut clients);
        if clients
            .iter_mut()
            .all(|c| c.world().resource::<NetLink>().is_connected() && avatars(c.world_mut()).len() == 2)
        {
            joined = true;
            break;
        }
    }
    assert!(joined, "both clients see both avatars");

    // No game-side score component is registered for replication; the engine's views are.
    for app in std::iter::once(&server).chain(clients.iter()) {
        let names: Vec<&str> = app
            .world()
            .resource::<ReplicationRegistry>()
            .components()
            .iter()
            .map(|c| c.name)
            .collect();
        assert!(names.iter().any(|n| n.ends_with("PlayerScoreView")), "{names:?}");
        let own: Vec<&str> = names
            .iter()
            .copied()
            .filter(|n| n.starts_with("net_game::"))
            .collect();
        assert_eq!(
            own,
            ["net_game::Avatar"],
            "the avatar is the game's only own component"
        );
    }

    clients[0].insert_resource(PadSource::Bot(Bot::new(0.0, 20, 900)));
    clients[1].insert_resource(PadSource::Bot(Bot::new(std::f32::consts::PI, 25, 900)));
    for _ in 0..900 + 300 {
        step(&mut server, &mut clients);
    }
    let hits = server.world().resource::<ShotLog>().hits;
    assert!(hits > 0, "the bots tagged each other");

    // The server's tally lives in the session's ledger, and each avatar shows its row.
    let server_views = score_views(&mut server);
    assert_eq!(server_views.len(), 2, "{server_views:?}");
    let kills: u32 = server_views.iter().map(|v| v.kills).sum();
    let deaths: u32 = server_views.iter().map(|v| v.deaths).sum();
    assert_eq!(kills, hits);
    assert_eq!(deaths, hits);
    let ledger_kills: u32 = server
        .world()
        .resource::<MatchRules>()
        .session
        .ledgers
        .rows
        .iter()
        .map(|r| r.kills)
        .sum();
    assert_eq!(ledger_kills, hits, "the ledger is the tally");
    let server_avatars = avatars(server.world_mut());
    for client in &mut clients {
        assert_eq!(
            score_views(client),
            server_views,
            "the client reads the server's rows"
        );
        let seen: Vec<_> = avatars(client.world_mut()).into_iter().map(|a| a.score).collect();
        let truth: Vec<_> = server_avatars.iter().map(|a| a.score.clone()).collect();
        assert_eq!(seen, truth);
        let has_state = client
            .world_mut()
            .query_filtered::<(), With<MatchStateEntity>>()
            .iter(client.world())
            .next()
            .is_some();
        assert!(has_state, "the client holds the match-state entity");
    }
}

/// The replicated team totals a process holds (the match-state entity's `TeamScoresView`).
fn team_view(app: &mut App) -> Option<TeamScoresView> {
    let mut q = app.world_mut().query::<&TeamScoresView>();
    q.iter(app.world()).next().cloned()
}

/// Kills per team (index = team id) summed from the rows a process sees.
fn team_kills(views: &[PlayerScoreView]) -> [u32; 3] {
    let mut kills = [0u32; 3];
    for v in views {
        if let Some(k) = kills.get_mut(usize::from(v.team)) {
            *k += v.kills;
        }
    }
    kills
}

/// GR-8.3: the template plays two teams through the match core. The joining players land on
/// different teams (`TeamId` 1 and 2), each at a `PlayerStart` of its own team, and after the bots
/// trade tags both clients hold the same rows, the same team totals and the same team labels as
/// the server, read from the replicated views only.
#[test]
fn net_game_two_clients_see_the_same_scores() {
    let net = LoopbackNet::new();
    let mut server = headless_app(Side::Server);
    let opts = link_options(manual(), ConditionerConfig::default());
    let addr = SocketAddr::from(([127, 0, 0, 1], 7778));
    let socket = net.bind(addr).expect("bind");
    server
        .world_mut()
        .resource_mut::<NetLink>()
        .host::<NetGameWire>(Box::new(socket), &opts)
        .expect("host");
    let key = server.world().resource::<NetLink>().server_key();
    let mut clients = Vec::new();
    for i in 0..2u64 {
        let mut app = headless_app(Side::Client);
        let opts = LinkOptions {
            server_key: key,
            name: format!("bot{i}"),
            ..link_options(manual(), ConditionerConfig::default())
        };
        let socket = net.bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("bind");
        app.world_mut()
            .resource_mut::<NetLink>()
            .connect::<NetGameWire>(Box::new(socket), addr, &opts)
            .expect("connect");
        clients.push(app);
    }
    let step = |server: &mut App, clients: &mut [App]| {
        server.update();
        for c in clients.iter_mut() {
            c.update();
        }
    };
    let mut joined = false;
    for _ in 0..600 {
        step(&mut server, &mut clients);
        if clients
            .iter_mut()
            .all(|c| c.world().resource::<NetLink>().is_connected() && avatars(c.world_mut()).len() == 2)
        {
            joined = true;
            break;
        }
    }
    assert!(joined, "both clients see both avatars");

    // Two teams, one player each, each standing on a start of its own team.
    let starts: Vec<(Vec3, u8)> = {
        let mut q = server
            .world_mut()
            .query_filtered::<(&Transform, &SpawnPoint), With<PlayerStart>>();
        q.iter(server.world())
            .map(|(t, p)| (t.translation, p.team))
            .collect()
    };
    assert_eq!(
        starts.len(),
        net_game::START_COUNT as usize,
        "the server placed its starts"
    );
    let placed: Vec<(u32, u8, Vec3)> = {
        let mut q = server.world_mut().query::<(&Avatar, &TeamId, &Transform)>();
        let mut v: Vec<_> = q
            .iter(server.world())
            .map(|(a, team, t)| (a.slot, team.0, t.translation))
            .collect();
        v.sort_by_key(|(slot, ..)| *slot);
        v
    };
    assert_eq!(
        placed.iter().map(|(_, team, _)| *team).collect::<Vec<_>>(),
        vec![1, 2],
        "the second player joins the other team"
    );
    for (slot, team, position) in &placed {
        assert!(
            starts
                .iter()
                .any(|(start, side)| side == team && (*start - *position).length() < 0.5),
            "avatar {slot} of team {team} started at {position}, not at a team {team} start ({starts:?})"
        );
    }

    clients[0].insert_resource(PadSource::Bot(Bot::new(0.0, 20, 900)));
    clients[1].insert_resource(PadSource::Bot(Bot::new(std::f32::consts::PI, 25, 900)));
    for _ in 0..900 + 300 {
        step(&mut server, &mut clients);
    }
    let hits = server.world().resource::<ShotLog>().hits;
    assert!(hits > 0, "the bots tagged each other");

    let server_views = score_views(&mut server);
    assert_eq!(
        server_views.iter().map(|v| v.team).collect::<Vec<_>>(),
        vec![1, 2],
        "one row per team: {server_views:?}"
    );
    let truth = {
        let session = &server.world().resource::<MatchRules>().session;
        team_scores_view(session)
    };
    assert_eq!(
        truth.labels.iter().map(|l| l.id).collect::<Vec<_>>(),
        vec![1, 2],
        "the session has two teams"
    );
    let server_kills = team_kills(&server_views);
    assert_eq!(
        server_kills.iter().sum::<u32>(),
        hits,
        "every tag is a kill on a team"
    );
    let mut seen = Vec::new();
    for client in &mut clients {
        let rows = score_views(client);
        assert_eq!(rows, server_views, "the client reads the server's rows");
        assert_eq!(team_kills(&rows), server_kills);
        let view = team_view(client).expect("the client holds the team totals");
        assert_eq!(
            view, truth,
            "the client's team totals and labels are the server's"
        );
        seen.push((rows, view));
    }
    assert_eq!(seen[0], seen[1], "both clients see the same scores");
}
