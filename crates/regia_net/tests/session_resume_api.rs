// SPDX-License-Identifier: MPL-2.0
//! Spec 120 § 10.3 (N102, ADR-8180): the host-visible surface of the resume fixes: the refused
//! token event, the flow-control signal and `try_send_game`, grace for an overflow on a silent
//! link, rebind addresses, and `connect_via` / `SocketOpener` resumes. Virtual time only.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use regia_net::model::PeerId;
use regia_net::session::lifecycle::{
    ClientSessionEvent, ClientSessionState, HostOptions, LinkId, PeerState, ServerSessionEvent,
    SessionServer, SessionSettings,
};
use regia_net::session::manifest::{ContentManifest, ManifestAsset};
use regia_net::session::net_session::ConnectOptions;
use regia_net::session::proto::{DisconnectReason, Hello, SessionMsg};
use regia_net::session::{NetSession, SocketOpener};
use regia_net::socket::{DatagramSocket, LoopbackNet};

fn manifest() -> ContentManifest {
    let base = vec![ManifestAsset {
        id: "levels/arena.level.ron".into(),
        kind: "level".into(),
        content_hash: 11,
    }];
    ContentManifest::build("0.5.0", "1.0.0", 1, 0xabc, &[("base", &base)])
}

fn host_opts() -> HostOptions {
    HostOptions {
        level: "levels/arena.level.ron".into(),
        ..HostOptions::default()
    }
}

fn hello(token: Option<[u8; 16]>) -> Vec<u8> {
    Hello {
        protocol_version: 1,
        protocol_hash: 0xabc,
        manifest: manifest(),
        player_names: vec!["ada".into()],
        password: None,
        auth: None,
        client_rate_bytes: 0,
        reconnect_token: token,
    }
    .encode()
    .expect("hello")
}

fn core() -> SessionServer {
    let mut s = SessionServer::new(SessionSettings::default(), host_opts(), manifest(), [7; 32]);
    s.set_dev_build(false);
    s
}

fn events(s: &mut SessionServer) -> Vec<ServerSessionEvent> {
    std::iter::from_fn(|| s.poll_event()).collect()
}

/// One peer in game on link 1.
fn in_game(s: &mut SessionServer) -> PeerId {
    let a: SocketAddr = "10.0.0.1:5000".parse().expect("addr");
    s.link_up(LinkId(1), a, 0);
    s.receive(LinkId(1), &hello(None), 0);
    let travel = s.travel_id();
    s.receive(
        LinkId(1),
        &SessionMsg::LevelReady { travel_id: travel }
            .encode()
            .expect("msg"),
        0,
    );
    s.receive(
        LinkId(1),
        &SessionMsg::InGame { tick: 0 }.encode().expect("msg"),
        0,
    );
    let peer = s.peer_of(LinkId(1)).expect("peer");
    assert_eq!(s.peer_state(peer), Some(PeerState::InGame));
    let _ = events(s);
    let _ = s.take_outgoing();
    peer
}

#[test]
fn a_token_that_matches_no_slot_raises_token_refused_and_joins_new() {
    let mut s = core();
    let peer = in_game(&mut s);
    let b: SocketAddr = "10.0.0.2:6000".parse().expect("addr");
    s.link_up(LinkId(2), b, 10);
    s.receive(LinkId(2), &hello(Some([0x5a; 16])), 10);
    let ev = events(&mut s);
    assert!(
        ev.iter()
            .any(|e| matches!(e, ServerSessionEvent::TokenRefused { link } if *link == LinkId(2))),
        "{ev:?}"
    );
    let newcomer = s.peer_of(LinkId(2)).expect("joined as a new player");
    assert_ne!(newcomer, peer);
    assert_eq!(
        s.link_of(peer),
        Some(LinkId(1)),
        "the forged token took nothing over"
    );
    assert!(s.take_quiet_closes().is_empty());
}

#[test]
fn a_takeover_closes_the_old_link_quietly() {
    let mut s = core();
    let a: SocketAddr = "10.0.0.1:5000".parse().expect("addr");
    s.link_up(LinkId(1), a, 0);
    s.receive(LinkId(1), &hello(None), 0);
    let token = s
        .take_outgoing()
        .into_iter()
        .find_map(|o| match SessionMsg::decode(&o.bytes) {
            Ok(SessionMsg::Welcome(w)) => Some(w.reconnect_token),
            _ => None,
        })
        .expect("welcome");
    let b: SocketAddr = "10.0.0.2:6000".parse().expect("addr");
    s.link_up(LinkId(2), b, 50);
    s.receive(LinkId(2), &hello(Some(token)), 50);
    assert_eq!(
        s.take_quiet_closes(),
        vec![LinkId(1)],
        "the old link closes without a goodbye"
    );
    assert!(
        !s.take_outgoing().iter().any(|o| o.link == LinkId(1)),
        "nothing more goes to the old link"
    );
    assert!(s.take_closes().is_empty(), "taken once");
}

#[test]
fn backpressure_pauses_streams_and_refuses_try_send_game() {
    let mut s = core();
    let peer = in_game(&mut s);
    s.send_stream(peer, "blob", vec![7; 200_000]).expect("stream");
    s.set_link_backpressure(LinkId(1), true);
    assert!(s.is_backpressured(peer));
    for t in 1..=30 {
        s.update(t * 16);
    }
    let chunks = |s: &mut SessionServer| {
        s.take_outgoing()
            .iter()
            .filter(|o| matches!(SessionMsg::decode(&o.bytes), Ok(SessionMsg::StreamChunk { .. })))
            .count()
    };
    assert_eq!(
        chunks(&mut s),
        0,
        "no stream chunks while the window is backed up"
    );
    assert_eq!(s.stream_bytes_sent(peer), 0);
    assert_eq!(s.try_send_game(peer, vec![1, 2, 3]), Err(vec![1, 2, 3]));
    s.set_link_backpressure(LinkId(1), false);
    for t in 31..=60 {
        s.update(t * 16);
    }
    assert!(chunks(&mut s) > 0, "streams resume once it drains");
    assert_eq!(s.try_send_game(peer, vec![4]), Ok(()));
    assert_eq!(
        s.peer_state(peer),
        Some(PeerState::InGame),
        "flow control never ends a peer"
    );
}

#[test]
fn an_overflow_on_a_silent_link_enters_grace_and_on_a_live_link_does_not() {
    let mut s = core();
    let peer = in_game(&mut s);
    // Silence past the interruption interval: the link is at fault.
    s.update(2_000);
    assert!(s.is_backpressured(peer), "an interrupted peer reads as backed up");
    s.link_down(LinkId(1), DisconnectReason::ReliableOverflow, 2_000);
    assert_eq!(s.peer_state(peer), Some(PeerState::Grace));
    assert!(events(&mut s).iter().any(|e| matches!(
        e,
        ServerSessionEvent::PlayerDropped {
            reason: DisconnectReason::ReliableOverflow,
            ..
        }
    )));

    // A healthy link that overflows is the game's fault: no grace.
    let mut s = core();
    let peer = in_game(&mut s);
    s.link_down(LinkId(1), DisconnectReason::ReliableOverflow, 10);
    assert_eq!(s.peer_state(peer), None);
}

#[test]
fn set_link_addr_moves_the_peer_address() {
    let mut s = core();
    let peer = in_game(&mut s);
    let moved: SocketAddr = "10.9.9.9:7000".parse().expect("addr");
    s.set_link_addr(LinkId(1), moved);
    assert_eq!(s.peer_addr(peer), Some(moved));
    assert_eq!(s.peer_rows()[0].addr, moved.to_string());
}

#[test]
fn resume_reopens_the_socket_kind_the_session_connected_with() {
    let net = LoopbackNet::new();
    let saddr: SocketAddr = "127.0.0.1:7811".parse().expect("addr");
    let mut server = NetSession::new(SessionSettings::default(), manifest());
    server.set_manual_clock(0);
    server
        .host_on(net.bind(saddr).expect("bind"), host_opts(), true)
        .expect("host");
    let opened = Arc::new(AtomicUsize::new(0));
    let opener: SocketOpener = {
        let net = net.clone();
        let opened = Arc::clone(&opened);
        Arc::new(move || {
            opened.fetch_add(1, Ordering::Relaxed);
            net.bind("127.0.0.1:0".parse().expect("addr"))
                .map(|s| Box::new(s) as Box<dyn DatagramSocket + Send>)
        })
    };
    let mut client = NetSession::new(SessionSettings::default(), manifest());
    client.set_manual_clock(0);
    assert!(!client.has_socket_opener());
    client
        .connect_via(opener, saddr, ConnectOptions::default())
        .expect("connect");
    assert!(client.has_socket_opener());
    assert_eq!(opened.load(Ordering::Relaxed), 1);
    let drive = |server: &mut NetSession, client: &mut NetSession| {
        for _ in 0..300 {
            server.advance_manual_clock(16);
            client.advance_manual_clock(16);
            client.update();
            server.update();
            if client.state() == ClientSessionState::InGame {
                break;
            }
        }
    };
    drive(&mut server, &mut client);
    assert_eq!(client.state(), ClientSessionState::InGame);
    let peer = client
        .take_client_events()
        .iter()
        .find_map(|e| match e {
            ClientSessionEvent::Welcomed(w) => Some(w.peer_id),
            _ => None,
        })
        .expect("welcomed");
    client.suspend();
    server.advance_manual_clock(16);
    server.update();
    assert_eq!(server.peer_state(peer), Some(PeerState::Grace));
    assert!(client.resume().expect("resume"), "a reconnect began");
    assert_eq!(
        opened.load(Ordering::Relaxed),
        2,
        "resume used the session's opener, not UDP"
    );
    drive(&mut server, &mut client);
    assert_eq!(client.state(), ClientSessionState::InGame);
    assert!(client.has_socket_opener(), "kept for the next suspend");
    let resumed = client.take_client_events().iter().find_map(|e| match e {
        ClientSessionEvent::Welcomed(w) => Some(w.peer_id),
        _ => None,
    });
    assert_eq!(resumed, Some(peer));
    // `connect_on` with no opener falls back to UDP (the old behavior) and says so.
    client.reset();
    client
        .connect_on(
            net.bind("127.0.0.1:0".parse().expect("addr")).expect("bind"),
            saddr,
            ConnectOptions::default(),
        )
        .expect("connect");
    assert!(!client.has_socket_opener());
}
