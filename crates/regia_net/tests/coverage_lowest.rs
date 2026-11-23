// SPDX-License-Identifier: MPL-2.0
//! Public `regia_net` functions that `cargo xtask coverage --crate regia_net --lowest 25` listed as
//! never run (ADR-3283): the `NetSession` play-in-editor and client calls, the stats overlay, lobby
//! event codes, crypto error codes and relevance stage names. In-memory network, manual clocks.

use std::net::SocketAddr;

use regia_ecs::prelude::World;
use regia_net::LoopbackNet;
use regia_net::crypto::CryptoError;
use regia_net::lobby::{LeaveReason, LobbyError, LobbyEvent, LobbyId, MemberId, RequestId};
use regia_net::model::NetPlayerId;
use regia_net::relevance::RelevanceStage;
use regia_net::session::NetSession;
use regia_net::session::hub::SimInput;
use regia_net::session::lifecycle::{ClientSessionState, HostOptions, SessionSettings};
use regia_net::session::manifest::ContentManifest;
use regia_net::session::net_session::{ConnectOptions, SessionError};
use regia_net::session::players::{NetPlayerStats, PlayerLinkStats, overlay_lines};
use regia_net::session::proto::DisconnectReason;

const STEP_MS: u64 = 10;

fn manifest() -> ContentManifest {
    ContentManifest::build("game", "1", 1, 0, &[])
}

fn level() -> HostOptions {
    HostOptions {
        level: "levels/a.level.ron".into(),
        ..HostOptions::default()
    }
}

#[test]
fn loopback_clients_join_a_local_host_and_show_in_the_overlay() {
    let mut idle = NetSession::new(SessionSettings::default(), manifest());
    assert_eq!(idle.session_id(), None);
    assert_eq!(
        idle.add_loopback_clients(1, SimInput::Idle),
        Err(SessionError::NotServer)
    );
    let mut world = World::new();
    assert_eq!(overlay_lines(&world), vec!["net: no session".to_owned()]);

    let mut host = NetSession::new(SessionSettings::default(), manifest());
    host.set_manual_clock(0);
    let id = host.host_local(level(), false).expect("host");
    assert_eq!(host.session_id(), Some(id));
    assert_eq!(host.add_loopback_clients(2, SimInput::Idle), Ok(2));
    assert_eq!(
        host.add_loopback_clients(300, SimInput::Idle),
        Err(SessionError::SimClientsLimit)
    );
    for _ in 0..200 {
        host.advance_manual_clock(STEP_MS);
        host.update();
    }
    let in_game = host
        .server()
        .map(|s| s.peer_rows().iter().filter(|r| r.state == "in_game").count())
        .unwrap_or_default();
    assert_eq!(in_game, 2, "both loopback clients joined");

    // Without the ECS plugin no player entities exist, so the session lists no player rows; the
    // overlay reads whatever rows the stats resource holds: one per joined peer here.
    let (rows, server_row) = host.player_link_stats();
    assert!(rows.is_empty() && server_row.is_none());
    let mut stats = NetPlayerStats::default();
    for peer in host.server().map(|s| s.peer_ids()).unwrap_or_default() {
        stats.players.insert(
            NetPlayerId { peer, local: 0 },
            PlayerLinkStats {
                ping_ms: 20.0,
                jitter_ms: 1.5,
                loss_to_player: 0.0,
                loss_from_player: None,
                simulated: false,
            },
        );
    }
    world.insert_resource(host);
    world.insert_resource(stats);
    let lines = overlay_lines(&world);
    assert_eq!(lines[0], "net: server, 2 players");
    assert_eq!(lines.len(), 3, "{lines:?}");
    assert!(lines[1..].iter().all(|l| l.contains("in_game")), "{lines:?}");
}

/// A client that loads its level itself (`auto_level_ready` off) joins on `level_loaded`, and a
/// suspend says goodbye with `suspended`.
#[test]
fn a_client_reports_its_level_and_can_suspend() {
    let net = LoopbackNet::new();
    let server_addr = SocketAddr::from(([127, 0, 0, 1], 7777));
    let mut server = NetSession::new(SessionSettings::default(), manifest());
    server.set_manual_clock(0);
    server
        .host_on(net.bind(server_addr).expect("bind"), level(), true)
        .expect("host");
    let mut client = NetSession::new(SessionSettings::default(), manifest());
    client.set_manual_clock(0);
    client.auto_level_ready = false;
    client
        .connect_on(
            net.bind(SocketAddr::from(([127, 0, 0, 2], 0))).expect("bind"),
            server_addr,
            ConnectOptions::default(),
        )
        .expect("connect");
    let step = |server: &mut NetSession, client: &mut NetSession| {
        client.advance_manual_clock(STEP_MS);
        server.advance_manual_clock(STEP_MS);
        client.update();
        server.update();
    };
    for _ in 0..500 {
        if client.state() == ClientSessionState::Loading {
            break;
        }
        step(&mut server, &mut client);
    }
    assert_eq!(client.state(), ClientSessionState::Loading);
    // Nothing happens until the game says the level is in.
    for _ in 0..20 {
        step(&mut server, &mut client);
    }
    assert_eq!(client.state(), ClientSessionState::Loading);
    let travel = client.client().map(|c| c.travel_id()).expect("client");
    client.level_loaded(travel);
    for _ in 0..200 {
        if client.state() == ClientSessionState::InGame {
            break;
        }
        step(&mut server, &mut client);
    }
    assert_eq!(client.state(), ClientSessionState::InGame);
    assert_eq!(
        client.session_id(),
        server.session_id(),
        "the client learned the session id"
    );

    client.suspend();
    for _ in 0..20 {
        step(&mut server, &mut client);
    }
    assert_eq!(
        client.state(),
        ClientSessionState::Disconnected(Some(DisconnectReason::Suspended))
    );
    // The level call on a server or an idle session does nothing.
    server.level_loaded(travel);
}

#[test]
fn lobby_events_name_their_kind_and_lobby() {
    let lobby = LobbyId(7);
    let left = LobbyEvent::Left {
        lobby,
        reason: LeaveReason::Kicked,
    };
    assert_eq!((left.kind(), left.lobby()), ("left", Some(lobby)));
    let invite = LobbyEvent::InviteAccepted {
        lobby,
        from: MemberId(3),
    };
    assert_eq!((invite.kind(), invite.lobby()), ("invite_accepted", Some(lobby)));
    let list = LobbyEvent::List {
        request: RequestId(1),
        result: Ok(Vec::new()),
    };
    assert_eq!((list.kind(), list.lobby()), ("list", None));
    let failed = LobbyEvent::Created {
        request: RequestId(2),
        result: Err(LobbyError::Unavailable),
    };
    assert_eq!((failed.kind(), failed.lobby()), ("created", None));
    let made = LobbyEvent::Entered {
        request: RequestId(3),
        result: Ok(lobby),
    };
    assert_eq!((made.kind(), made.lobby()), ("entered", Some(lobby)));
}

fn is_code(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

#[test]
fn crypto_error_codes_are_distinct_protocol_codes() {
    let codes: Vec<&str> = [
        CryptoError::Auth,
        CryptoError::Replay,
        CryptoError::Malformed,
        CryptoError::Exhausted,
        CryptoError::KeyMismatch,
        CryptoError::TooLarge,
        CryptoError::Unavailable,
        CryptoError::WeakKey,
    ]
    .into_iter()
    .map(CryptoError::code)
    .collect();
    assert!(codes.iter().all(|c| is_code(c)), "{codes:?}");
    let unique: std::collections::BTreeSet<&str> = codes.iter().copied().collect();
    assert_eq!(unique.len(), codes.len());
    assert_eq!(CryptoError::KeyMismatch.code(), "server_key_mismatch");
}

#[test]
fn relevance_stage_names_are_distinct_protocol_codes() {
    let names: Vec<&str> = [
        RelevanceStage::Owner,
        RelevanceStage::SentToAll,
        RelevanceStage::OwnerOnly,
        RelevanceStage::Visibility,
        RelevanceStage::Group,
        RelevanceStage::Link,
        RelevanceStage::Spatial,
        RelevanceStage::Custom,
        RelevanceStage::Grant,
    ]
    .into_iter()
    .map(RelevanceStage::as_str)
    .collect();
    assert!(names.iter().all(|n| is_code(n)), "{names:?}");
    let unique: std::collections::BTreeSet<&str> = names.iter().copied().collect();
    assert_eq!(unique.len(), names.len());
}
