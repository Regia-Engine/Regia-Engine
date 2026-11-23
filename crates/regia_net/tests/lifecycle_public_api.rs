// SPDX-License-Identifier: MPL-2.0
//! The public functions of `session::lifecycle` that no test called (ADR-3283, measured with
//! `cargo xtask coverage --crate regia_net`): broadcasts, admin commands, kicks, the ban store,
//! platform ids, names, slot counts, drains, closes and a failed level load. Loopback hub, simulated
//! time, no sockets.

use std::net::SocketAddr;

use regia_net::model::{NetPlayerId, PeerId};
use regia_net::session::bans::{BanKind, BanStore};
use regia_net::session::hub::SessionHub;
use regia_net::session::lifecycle::{
    AuthRequestId, AuthResult, ClientSessionEvent, ClientSessionState, HostOptions, PeerAddr, PeerState,
    SessionAuthenticator, SessionClient, SessionSettings,
};
use regia_net::session::manifest::ContentManifest;
use regia_net::session::proto::{DisconnectReason, Hello, SessionMsg};

fn manifest() -> ContentManifest {
    ContentManifest::build("0.5.0", "1.2.0", 1, 0xabc, &[])
}

fn hello(names: &[&str]) -> Hello {
    Hello {
        protocol_version: 1,
        protocol_hash: 0xabc,
        manifest: manifest(),
        player_names: names.iter().map(|s| (*s).to_owned()).collect(),
        password: None,
        auth: None,
        client_rate_bytes: 0,
        reconnect_token: None,
    }
}

fn hub_with(opts: HostOptions) -> SessionHub {
    let mut hub = SessionHub::new(
        SessionSettings::default(),
        HostOptions {
            level: "levels/arena.level.ron".into(),
            ..opts
        },
        manifest(),
    );
    hub.server.set_dev_build(false);
    hub
}

fn join(hub: &mut SessionHub, h: Hello) -> usize {
    let c = hub.add_client(h);
    assert!(
        hub.run_until(600, |h| h.clients.client(c).in_game()),
        "{:?}",
        hub.clients.client(c).state()
    );
    hub.run(3);
    c
}

fn peer_of(hub: &SessionHub, c: usize) -> PeerId {
    hub.server
        .peer_of(hub.clients.client(c).link)
        .expect("a joined client has a peer")
}

fn client_events(hub: &SessionHub, c: usize) -> &[ClientSessionEvent] {
    &hub.clients.client(c).log
}

#[test]
fn broadcast_game_reaches_every_in_game_peer() {
    let mut hub = hub_with(HostOptions::default());
    let a = join(&mut hub, hello(&["Ann"]));
    let b = join(&mut hub, hello(&["Bob"]));
    assert_eq!(hub.server.broadcast_game(b"round 2"), 2);
    hub.run(5);
    for c in [a, b] {
        assert!(
            client_events(&hub, c)
                .iter()
                .any(|e| matches!(e, ClientSessionEvent::Game(bytes) if bytes == b"round 2")),
            "client {c}"
        );
    }
}

#[test]
fn names_slots_and_peer_lookups() {
    let mut hub = hub_with(HostOptions::default());
    let c = join(&mut hub, hello(&["Ann", "Bea"]));
    let peer = peer_of(&hub, c);
    assert_eq!(hub.server.players_in_use(), 2);
    assert_eq!(
        hub.server.player_name(NetPlayerId { peer, local: 0 }),
        Some("Ann")
    );
    assert_eq!(
        hub.server.player_name(NetPlayerId { peer, local: 1 }),
        Some("Bea")
    );
    assert_eq!(hub.server.player_name(NetPlayerId { peer, local: 7 }), None);
    assert_eq!(hub.server.peer_state(peer), Some(PeerState::InGame));
    assert_eq!(
        hub.server.peer_platform_id(peer),
        None,
        "no authenticator, no platform id"
    );
    assert_eq!(
        hub.server
            .peer_of(regia_net::session::lifecycle::LinkId(u32::MAX)),
        None
    );
}

#[test]
fn the_authenticators_platform_id_is_kept() {
    struct Steam;
    impl SessionAuthenticator for Steam {
        fn check(&mut self, _: AuthRequestId, h: &Hello, _: &PeerAddr) -> AuthResult {
            AuthResult::Accept {
                platform_id: Some(format!("steam:{}", h.player_names[0])),
                reserved: false,
            }
        }
    }
    let mut hub = hub_with(HostOptions::default());
    hub.server.set_authenticator(Box::new(Steam));
    let c = join(&mut hub, hello(&["Eve"]));
    let peer = peer_of(&hub, c);
    assert_eq!(hub.server.peer_platform_id(peer), Some("steam:Eve"));
}

#[test]
fn remote_admin_commands_queue_and_get_answers() {
    let mut hub = hub_with(HostOptions {
        admin_token: Some("tok3n".into()),
        ..HostOptions::default()
    });
    let c = join(&mut hub, hello(&["Ann"]));
    let peer = peer_of(&hub, c);
    assert!(!hub.server.is_admin(peer));
    // A command before the login is refused and not queued.
    hub.clients
        .client_mut(c)
        .core
        .send_admin(SessionMsg::AdminCommand {
            line: "status".into(),
        });
    hub.run(5);
    assert!(hub.server.take_admin_commands().is_empty());
    hub.clients.client_mut(c).core.send_admin(SessionMsg::AdminLogin {
        token: "tok3n".into(),
    });
    hub.run(5);
    assert!(hub.server.is_admin(peer));
    hub.clients
        .client_mut(c)
        .core
        .send_admin(SessionMsg::AdminCommand {
            line: "players".into(),
        });
    hub.run(5);
    assert_eq!(
        hub.server.take_admin_commands(),
        vec![(peer, "players".to_owned())]
    );
    assert!(hub.server.take_admin_commands().is_empty(), "taken once");
    hub.server.admin_reply(peer, vec!["1 player".into()]);
    hub.run(5);
    let outputs: Vec<&Vec<String>> = client_events(&hub, c)
        .iter()
        .filter_map(|e| match e {
            ClientSessionEvent::AdminOutput(lines) => Some(lines),
            _ => None,
        })
        .collect();
    assert_eq!(
        outputs,
        [
            &vec!["not an admin".to_owned()],
            &vec!["admin: logged in".to_owned()],
            &vec!["1 player".to_owned()]
        ]
    );
}

#[test]
fn an_admin_login_over_a_plain_link_is_refused() {
    let mut hub = hub_with(HostOptions {
        admin_token: Some("tok3n".into()),
        ..HostOptions::default()
    });
    let c = hub.add_client(hello(&["Ann"]));
    // The link comes up on the first step; mark it before the Hello is read.
    hub.step();
    let link = hub.clients.client(c).link;
    hub.server.mark_link_plain(link);
    assert!(hub.run_until(600, |h| h.clients.client(c).in_game()));
    hub.run(3);
    let peer = peer_of(&hub, c);
    hub.clients.client_mut(c).core.send_admin(SessionMsg::AdminLogin {
        token: "tok3n".into(),
    });
    hub.run(5);
    assert!(!hub.server.is_admin(peer), "the token crossed a plain wire");
    assert!(client_events(&hub, c).iter().any(|e| matches!(
        e,
        ClientSessionEvent::AdminOutput(lines) if lines.first().is_some_and(|l| l.contains("encrypted"))
    )));
}

#[test]
fn kick_peer_and_disconnect_peer_end_the_client_with_their_reason() {
    let mut hub = hub_with(HostOptions::default());
    let a = join(&mut hub, hello(&["Ann"]));
    let b = join(&mut hub, hello(&["Bob"]));
    let (pa, pb) = (peer_of(&hub, a), peer_of(&hub, b));
    assert!(hub.server.kick_peer(pa, "afk"));
    assert!(!hub.server.kick_peer(PeerId(9999), "nobody"), "unknown peer");
    hub.server.disconnect_peer(pb, DisconnectReason::Flood);
    assert!(hub.run_until(300, |h| {
        h.clients.client(a).disconnect_reason().is_some() && h.clients.client(b).disconnect_reason().is_some()
    }));
    assert_eq!(
        hub.clients.client(a).disconnect_reason(),
        Some(&DisconnectReason::Kicked("afk".into()))
    );
    assert_eq!(
        hub.clients.client(b).disconnect_reason(),
        Some(&DisconnectReason::Flood)
    );
    assert_eq!(hub.server.peer_state(pa), None, "retired, no grace");
    assert_eq!(hub.server.players_in_use(), 0);
}

#[test]
fn a_disconnected_peers_link_is_closed_after_its_goodbye() {
    let mut hub = hub_with(HostOptions::default());
    let c = join(&mut hub, hello(&["Ann"]));
    let link = hub.clients.client(c).link;
    let peer = peer_of(&hub, c);
    hub.server
        .disconnect_peer(peer, DisconnectReason::ServerShuttingDown);
    let mut now = hub.now_ms;
    let mut closed = Vec::new();
    for _ in 0..16 {
        now += 10;
        hub.server.update(now);
        closed = hub.server.take_closes();
        if !closed.is_empty() {
            break;
        }
    }
    assert_eq!(
        closed,
        vec![link],
        "the goodbye's link is closed after a few updates"
    );
    assert!(hub.server.take_closes().is_empty(), "once");
}

#[test]
fn a_replaced_ban_store_is_enforced() {
    let mut hub = hub_with(HostOptions::default());
    let mut store = BanStore::in_memory();
    store
        .add(BanKind::Address, "10.9.0.0/16", None, "spam", 0)
        .expect("ban");
    hub.server.set_bans(store);
    assert_eq!(hub.server.bans().list().entries.len(), 1);
    let c = hub.add_client(hello(&["Mal"]));
    hub.clients.set_addr(c, SocketAddr::from(([10, 9, 3, 4], 5000)));
    assert!(hub.run_until(600, |h| h.clients.client(c).disconnect_reason().is_some()));
    assert!(matches!(
        hub.clients.client(c).disconnect_reason(),
        Some(DisconnectReason::Banned(_))
    ));
}

#[test]
fn draining_is_reported() {
    let mut hub = hub_with(HostOptions::default());
    let _ = join(&mut hub, hello(&["Ann"]));
    assert!(!hub.server.is_draining());
    hub.server.drain();
    assert!(hub.server.is_draining());
}

#[test]
fn a_level_that_fails_to_load_ends_the_client_and_tells_the_server() {
    let mut hub = hub_with(HostOptions::default());
    let c = hub.add_client(hello(&["Ann"]));
    // Never finish loading on its own.
    hub.clients.client_mut(c).load_ms = None;
    assert!(hub.run_until(600, |h| *h.clients.client(c).state()
        == ClientSessionState::Loading));
    let now = hub.now_ms;
    hub.clients
        .client_mut(c)
        .core
        .level_failed("levels/arena.level.ron: missing", now);
    assert_eq!(
        hub.clients.client(c).core.state(),
        &ClientSessionState::Disconnected(Some(DisconnectReason::LevelLoadFailed(
            "levels/arena.level.ron: missing".into()
        )))
    );
    hub.run(10);
    assert_eq!(
        hub.server.players_in_use(),
        0,
        "the server dropped the peer on its goodbye"
    );
}

#[test]
fn a_client_asks_its_driver_to_close_once() {
    let mut client = SessionClient::new(SessionSettings::default());
    assert!(client.connect(hello(&["Ann"]), 0));
    client.link_up(0);
    assert!(
        client
            .take_outgoing()
            .iter()
            .any(|(_, bytes)| matches!(SessionMsg::decode(bytes), Ok(SessionMsg::Hello(_))))
    );
    assert!(!client.take_close());
    client.disconnect(DisconnectReason::ClientQuit, 10);
    let mut asked = 0;
    for step in 0..10 {
        client.update(20 + step * 10);
        if client.take_close() {
            asked += 1;
        }
    }
    assert_eq!(asked, 1, "take_close is true exactly once");
    assert_eq!(
        client.state(),
        &ClientSessionState::Disconnected(Some(DisconnectReason::ClientQuit))
    );
}
