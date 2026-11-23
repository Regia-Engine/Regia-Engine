// SPDX-License-Identifier: MPL-2.0
//! Spec 89 § 11: Steam lobbies and the Steam sockets transport against the shared mock network.
//! A spec 77 session hosted on a `SteamSocket` is joined from another mock user and reaches
//! `in_game`; two apps with `SteamLobbyPlugin` matchmake, launch over Steam sockets and
//! auto-connect. No SDK, no wall time.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use regia_app::prelude::*;
use regia_input::InputContexts;
use regia_net::lobby::{
    LobbyBackend, LobbyError, LobbyEvent, LobbyId, LobbyInfo, LobbyMember, LobbyQuery, LobbyServer,
    LobbySpec, MatchState, MemberId, RequestId,
};
use regia_net::session::lifecycle::{ClientSessionState, HostOptions};
use regia_net::session::net_session::ConnectOptions;
use regia_net::{DatagramSocket, Lobbies, NetSession, NetSessionPlugin};
use regia_platform::AppLifecycle;
use regia_steam::{
    MockSteam, MockSteamNet, SteamAccept, SteamBackend, SteamId, SteamLobbyPlugin, SteamPlugin,
    SteamSettings, SteamSocket, steam_id_of, steam_peer_addr,
};

fn user(net: &MockSteamNet, id: u64, name: &str) -> Arc<MockSteam> {
    let mock = MockSteam::shared();
    mock.join_net(net, SteamId(id), name);
    mock
}

#[test]
fn a_session_runs_over_steam_sockets() {
    let net = MockSteamNet::new();
    let host = user(&net, 76_561_190_000_000_001, "Ann");
    let guest = user(&net, 76_561_190_000_000_002, "Bob");
    let host_socket = SteamSocket::open(host.p2p().expect("p2p"), 0).expect("socket");
    let host_addr = host_socket.local_addr().expect("addr");
    assert_eq!(steam_id_of(host_addr), Some((SteamId(76_561_190_000_000_001), 0)));
    let host_stats = host_socket.stats_handle();

    let mut server = NetSession::default();
    server.set_manual_clock(0);
    server
        .host_on(
            host_socket,
            HostOptions {
                level: "levels/arena.level.ron".into(),
                ..HostOptions::default()
            },
            false,
        )
        .expect("host");
    let mut client = NetSession::default();
    client.set_manual_clock(0);
    client
        .connect_on(
            SteamSocket::open(guest.p2p().expect("p2p"), 0).expect("socket"),
            steam_peer_addr(SteamId(76_561_190_000_000_001), 0),
            ConnectOptions::default(),
        )
        .expect("connect");
    for _ in 0..200 {
        server.advance_manual_clock(16);
        client.advance_manual_clock(16);
        client.update();
        server.update();
        if client.state() == ClientSessionState::InGame {
            break;
        }
    }
    assert_eq!(client.state(), ClientSessionState::InGame);
    let stats = *host_stats.lock().expect("stats");
    assert!(stats.sent > 0 && stats.received > 0, "{stats:?}");
    assert_eq!(stats.send_errors, 0);
}

#[test]
fn sends_to_non_steam_addresses_are_refused_and_listed_accept_filters() {
    let net = MockSteamNet::new();
    let a = user(&net, 11, "a");
    let b = user(&net, 12, "b");
    let c = user(&net, 13, "c");
    let mut sa = SteamSocket::open(a.p2p().expect("p2p"), 3).expect("socket");
    let mut sb = SteamSocket::open(b.p2p().expect("p2p"), 3).expect("socket");
    let mut sc = SteamSocket::open(c.p2p().expect("p2p"), 3).expect("socket");
    let err = sa
        .send_to(b"x", "127.0.0.1:9".parse().expect("addr"))
        .expect_err("not a Steam peer");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    let too_big = vec![0u8; regia_net::socket::MAX_DATAGRAM_BYTES + 1];
    assert!(sa.send_to(&too_big, steam_peer_addr(SteamId(12), 3)).is_err());
    // b accepts only a.
    b.p2p()
        .expect("p2p")
        .set_accept(SteamAccept::Listed([SteamId(11)].into_iter().collect()));
    sa.send_to(b"from a", steam_peer_addr(SteamId(12), 3))
        .expect("send");
    sc.send_to(b"from c", steam_peer_addr(SteamId(12), 3))
        .expect("send");
    let mut buf = [0u8; 64];
    let (n, from) = sb.recv_from(&mut buf).expect("io").expect("datagram");
    assert_eq!(&buf[..n], b"from a");
    assert_eq!(from, steam_peer_addr(SteamId(11), 3));
    assert!(
        sb.recv_from(&mut buf).expect("io").is_none(),
        "c's session was rejected"
    );
    assert_eq!(net.counters(), (1, 1));
    // An offline peer: the send fails and says so (spec 120 § 10.8, adversarial row 15), so the
    // endpoint counts a socket error instead of thinking the datagram left; it never panics.
    net.set_online(SteamId(12), false);
    assert!(
        sa.send_to(b"lost", steam_peer_addr(SteamId(12), 3)).is_err(),
        "a refused send is surfaced"
    );
    assert_eq!(sa.stats().send_errors, 1);
}

/// Counts `lobby()` calls. A quiet joined lobby must be snapshotted once, not every pump.
struct CountingLobbies {
    lobby_calls: AtomicU64,
    name_lists: AtomicU64,
    events: Mutex<Vec<LobbyEvent>>,
    info: Mutex<LobbyInfo>,
}

impl CountingLobbies {
    fn new() -> Self {
        Self {
            lobby_calls: AtomicU64::new(0),
            name_lists: AtomicU64::new(0),
            events: Mutex::new(Vec::new()),
            info: Mutex::new(LobbyInfo {
                id: LobbyId(7),
                owner: MemberId(1),
                visibility: regia_net::lobby::LobbyVisibility::Public,
                max_members: 8,
                joinable: true,
                members: vec![LobbyMember {
                    id: MemberId(1),
                    name: "Ann".into(),
                    data: std::collections::BTreeMap::new(),
                }],
                member_count: 1,
                data: std::collections::BTreeMap::new(),
                server: None,
            }),
        }
    }
}

impl LobbyBackend for CountingLobbies {
    fn name(&self) -> &'static str {
        "counting"
    }
    fn local_member(&self) -> Option<MemberId> {
        Some(MemberId(1))
    }
    fn create(&self, _spec: &LobbySpec) -> Result<RequestId, LobbyError> {
        self.events.lock().expect("events").push(LobbyEvent::Created {
            request: RequestId(1),
            result: Ok(LobbyId(7)),
        });
        Ok(RequestId(1))
    }
    fn join(&self, _lobby: LobbyId) -> Result<RequestId, LobbyError> {
        Err(LobbyError::Unavailable)
    }
    fn leave(&self, _lobby: LobbyId) {}
    fn request_list(&self, _query: &LobbyQuery) -> Result<RequestId, LobbyError> {
        Err(LobbyError::Unavailable)
    }
    fn lobby(&self, lobby: LobbyId) -> Option<LobbyInfo> {
        self.lobby_calls.fetch_add(1, Ordering::Relaxed);
        // Building the member-name list is the cost `poll` must not repeat while ids are stable.
        self.name_lists.fetch_add(1, Ordering::Relaxed);
        let mut info = self.info.lock().expect("info").clone();
        info.id = lobby;
        Some(info)
    }
    fn set_data(&self, _lobby: LobbyId, _key: &str, _value: Option<&str>) -> Result<(), LobbyError> {
        Ok(())
    }
    fn set_member_data(&self, _lobby: LobbyId, _key: &str, _value: Option<&str>) -> Result<(), LobbyError> {
        Ok(())
    }
    fn set_joinable(&self, _lobby: LobbyId, _joinable: bool) -> Result<(), LobbyError> {
        Ok(())
    }
    fn set_server(&self, _lobby: LobbyId, _server: LobbyServer) -> Result<(), LobbyError> {
        Ok(())
    }
    fn send_chat(&self, _lobby: LobbyId, _text: &str) -> Result<(), LobbyError> {
        Ok(())
    }
    fn poll(&self) -> Vec<LobbyEvent> {
        std::mem::take(&mut *self.events.lock().expect("events"))
    }
    fn still_in(&self, _lobby: LobbyId) -> bool {
        true
    }
}

#[test]
fn a_quiet_lobby_snapshot_is_taken_once() {
    let backend = Arc::new(CountingLobbies::new());
    let mut lobbies = Lobbies::new(backend.clone());
    // No NetSession here: opt in to an empty identity (ADR-7881).
    lobbies.allow_unidentified(true);
    lobbies.create(lobbies.default_spec()).expect("create");
    lobbies.pump();
    assert_eq!(lobbies.current(), Some(LobbyId(7)));
    assert_eq!(backend.lobby_calls.load(Ordering::Relaxed), 1);
    assert_eq!(
        lobbies
            .snapshot()
            .and_then(|s| s.members.first().map(|m| m.name.clone())),
        Some("Ann".into())
    );
    lobbies.pump();
    lobbies.pump();
    assert_eq!(
        backend.lobby_calls.load(Ordering::Relaxed),
        1,
        "a lobby whose members and data do not change must not be rebuilt every frame"
    );
    assert_eq!(
        backend.name_lists.load(Ordering::Relaxed),
        1,
        "poll must not allocate a fresh member-name list when the id set is unchanged"
    );

    backend
        .info
        .lock()
        .expect("info")
        .data
        .insert("map".into(), "forest".into());
    backend
        .events
        .lock()
        .expect("events")
        .push(LobbyEvent::DataChanged {
            lobby: LobbyId(7),
            member: None,
        });
    lobbies.pump();
    assert_eq!(
        lobbies.snapshot().and_then(|s| s.data.get("map").cloned()),
        Some("forest".into()),
        "a data change still refreshes the snapshot"
    );
    assert_eq!(backend.lobby_calls.load(Ordering::Relaxed), 2);
    lobbies.pump();
    assert_eq!(backend.lobby_calls.load(Ordering::Relaxed), 2);
}

fn app(mock: &Arc<MockSteam>) -> App {
    let mut app = App::new();
    app.insert_resource(SteamSettings::default())
        .insert_resource(AppLifecycle::default())
        .insert_resource(InputContexts::default());
    app.add_plugins(
        SteamPlugin::from_project_settings().with_backend(Arc::clone(mock) as Arc<dyn SteamBackend>),
    );
    let mut session = NetSession::default();
    session.set_manual_clock(0);
    app.insert_resource(session);
    app.add_plugins((NetSessionPlugin, SteamLobbyPlugin::default()));
    app.finish_plugins();
    app
}

fn step(apps: &mut [&mut App], n: usize) {
    for _ in 0..n {
        for app in apps.iter_mut() {
            app.world_mut()
                .resource_mut::<NetSession>()
                .advance_manual_clock(16);
            app.update();
        }
    }
}

#[test]
fn steam_lobby_plugin_matchmakes_launches_and_connects() {
    let net = MockSteamNet::new();
    let ann = user(&net, 76_561_190_000_000_010, "Ann");
    let bob = user(&net, 76_561_190_000_000_020, "Bob");
    let mut host = app(&ann);
    let mut guest = app(&bob);
    step(&mut [&mut host, &mut guest], 1);
    for app in [&host, &guest] {
        let l = app.world().resource::<Lobbies>();
        assert_eq!(
            l.backend().name(),
            "local",
            "the mock's lobbies are the local backend"
        );
        assert_eq!(l.transport_name(), Some("steam"));
    }
    {
        let mut l = host.world_mut().resource_mut::<Lobbies>();
        let mut req = l.default_request();
        req.rounds.truncate(1);
        l.start_matchmaking(req).expect("matchmaking");
    }
    step(&mut [&mut host], 3);
    {
        let mut l = guest.world_mut().resource_mut::<Lobbies>();
        let req = l.default_request();
        l.start_matchmaking(req).expect("matchmaking");
    }
    step(&mut [&mut host, &mut guest], 3);
    let lobby = guest
        .world()
        .resource::<Lobbies>()
        .current()
        .expect("guest in lobby");
    let server = host
        .world_mut()
        .resource_scope(|world, mut lobbies: Mut<Lobbies>| {
            let mut session = world.resource_mut::<NetSession>();
            lobbies.launch_hosting(&mut session, HostOptions::default())
        })
        .expect("launch");
    assert_eq!(server, LobbyServer::Steam(76_561_190_000_000_010));
    step(&mut [&mut host, &mut guest], 150);
    let g = guest.world().resource::<Lobbies>();
    assert_eq!(*g.matchmaker().state(), MatchState::Launched { lobby, server });
    assert_eq!(
        g.snapshot()
            .and_then(|s| s.data.get("regia.state"))
            .map(String::as_str),
        Some("in_game"),
        "the owner saw the guest reach the game"
    );
    assert_eq!(
        guest.world().resource::<NetSession>().state(),
        ClientSessionState::InGame
    );
    assert_eq!(
        guest.world().resource::<NetSession>().server_addr(),
        Some(steam_peer_addr(SteamId(76_561_190_000_000_010), 0))
    );
}

#[test]
fn without_steam_lobbies_are_unavailable() {
    let mock = MockSteam::shared_unavailable();
    let mut app = app(&mock);
    step(&mut [&mut app], 2);
    let mut l = app.world_mut().resource_mut::<Lobbies>();
    assert_eq!(l.backend().name(), "none");
    let req = l.default_request();
    assert_eq!(
        l.start_matchmaking(req).map_err(|e| e.code()),
        Err("lobby_unavailable")
    );
}

#[test]
fn a_send_goes_out_on_the_channel_in_the_destination_address() {
    let net = MockSteamNet::new();
    let a = user(&net, 21, "a");
    let b = user(&net, 22, "b");
    let mut sa = SteamSocket::open(a.p2p().expect("p2p"), 0).expect("socket");
    let mut sb7 = SteamSocket::open(b.p2p().expect("p2p"), 7).expect("socket");
    let mut sb0 = SteamSocket::open(b.p2p().expect("p2p"), 0).expect("socket");
    sa.send_to(b"seven", steam_peer_addr(SteamId(22), 7))
        .expect("send");
    let mut buf = [0u8; 16];
    let (n, from) = sb7.recv_from(&mut buf).expect("io").expect("on channel 7");
    assert_eq!(&buf[..n], b"seven");
    assert_eq!(from, steam_peer_addr(SteamId(21), 7));
    assert!(
        sb0.recv_from(&mut buf).expect("io").is_none(),
        "nothing on channel 0"
    );
}

#[test]
fn the_invalid_id_zero_has_an_address_but_no_peer() {
    let addr = steam_peer_addr(SteamId(0), 1);
    assert_eq!(steam_id_of(addr), None);
}
