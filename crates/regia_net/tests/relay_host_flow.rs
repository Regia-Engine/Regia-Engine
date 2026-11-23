// SPDX-License-Identifier: MPL-2.0
//! The relay host and client flow (spec 120 § 10.6 and § 10.7, ADR-8183, DO27 need
//! "net-relay-many-clients", lane 2 of 2): the host registers and keeps its lobby alive from its
//! game socket, behind a simulated symmetric NAT (`SimNat`: address-and-port-dependent mapping
//! and filtering), and clients join by code. Their direct attempt cannot pass the host's NAT, so
//! each falls back to its own relay allocation within 1.5 s and plays on it. A protocol 1 relay
//! ends the join with `relay_too_old` instead of a hang, and `NetSession::connect(JoinCode)`
//! joins end to end over real UDP.
//!
//! Time is a step counter on the in-memory network (and on 127.0.0.1 for the real-UDP test); every
//! wait is a bounded loop on a condition. Nothing sleeps and no assertion reads the wall clock.

mod common;

use std::collections::{BTreeSet, HashMap};
use std::io;
use std::net::{Ipv4Addr, SocketAddr};

use common::{Duel, DuelMsg, Team};
use regia_net::CryptoMode;
use regia_net::crypto::ServerKey;
use regia_net::prelude::*;
use regia_net::relay::{
    NatFiltering, NatMapping, RelayConfig, RelayCore, RelayServer, SimNat, relay_version,
};
use regia_net::server_host::{
    JoinPath, JoinRoute, JoinTicket, RELAY_FALLBACK_MS, RelayRegistrar, relay_split,
};
use regia_net::session::{
    ClientSessionEvent, ClientSessionState, ConnectOptions, ConnectTarget, DisconnectReason, HostOptions,
    NetSession,
};
use regia_net::socket::LoopbackSocket;

const STEP_MS: u64 = 20;
const CLIENTS: usize = 16;
const RELAY_IP: [u8; 4] = [192, 0, 2, 1];
const HOST_PUBLIC_IP: Ipv4Addr = Ipv4Addr::new(198, 51, 100, 7);

fn relay_addr() -> SocketAddr {
    SocketAddr::from((RELAY_IP, 7800))
}

/// A private socket behind a [`SimNat`] on the in-memory network: each mapping the NAT makes is a
/// public socket on its public IP; inbound datagrams pass only where the NAT's filter lets them.
struct NatSocket {
    net: LoopbackNet,
    nat: SimNat,
    private: SocketAddr,
    public: HashMap<u16, LoopbackSocket>,
    filtered: u64,
}

impl NatSocket {
    fn new(net: &LoopbackNet, nat: SimNat, private: SocketAddr) -> Self {
        Self {
            net: net.clone(),
            nat,
            private,
            public: HashMap::new(),
            filtered: 0,
        }
    }
}

impl DatagramSocket for NatSocket {
    fn send_to(&mut self, bytes: &[u8], dest: SocketAddr) -> io::Result<()> {
        let public = self.nat.outbound(dest);
        if !self.public.contains_key(&public.port()) {
            let socket = self.net.bind(public)?;
            self.public.insert(public.port(), socket);
        }
        match self.public.get_mut(&public.port()) {
            Some(socket) => socket.send_to(bytes, dest),
            None => Ok(()),
        }
    }

    fn recv_from(&mut self, buf: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
        let mut ports: Vec<u16> = self.public.keys().copied().collect();
        ports.sort_unstable();
        for port in ports {
            let Some(socket) = self.public.get_mut(&port) else {
                continue;
            };
            while let Some((n, from)) = socket.recv_from(buf)? {
                if self.nat.inbound(from, port) {
                    return Ok(Some((n, from)));
                }
                self.filtered += 1;
            }
        }
        Ok(None)
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.private)
    }
}

fn symmetric_nat() -> SimNat {
    SimNat::new(
        NatMapping::AddressAndPortDependent,
        NatFiltering::AddressAndPortDependent,
        HOST_PUBLIC_IP,
    )
}

/// A protocol 2 relay on the in-memory network with `ports` allocation ports.
fn relay_v2(net: &LoopbackNet, ports: u16) -> RelayServer<LoopbackSocket> {
    let control = net.bind(relay_addr()).expect("bind relay");
    let pool = (0..ports)
        .map(|i| {
            net.bind(SocketAddr::from((RELAY_IP, 50_000 + i)))
                .expect("bind allocation port")
        })
        .collect();
    RelayServer::from_core(
        control,
        pool,
        RelayCore::with_config(21, [4; 32], RelayConfig::default()),
    )
}

/// A relay that speaks protocol 1 only, the way the v0.5.0 relay does: it never reads the
/// versioned framing and has no allocation ports.
struct RelayV1 {
    socket: LoopbackSocket,
    core: RelayCore,
}

impl RelayV1 {
    fn new(net: &LoopbackNet) -> Self {
        Self {
            socket: net.bind(relay_addr()).expect("bind relay"),
            core: RelayCore::with_secret(23, [5; 32]),
        }
    }

    fn update(&mut self, now_ms: u64) {
        let mut buf = [0u8; 2048];
        while let Some((n, from)) = self.socket.recv_from(&mut buf).expect("io") {
            if relay_version(&buf[..n]).is_some_and(|v| v >= 2) {
                continue;
            }
            for (bytes, to) in self.core.handle(from, &buf[..n], now_ms) {
                self.socket.send_to(&bytes, to).expect("send");
            }
        }
    }
}

/// The host: a sealed `NetServer` on the game half of its NAT'd socket, the registrar on the
/// control half of the same socket.
struct Host {
    server: NetServer<Duel, regia_net::server_host::RelayGamePort<NatSocket>>,
    registrar: RelayRegistrar,
}

impl Host {
    fn new(net: &LoopbackNet) -> Self {
        let socket = NatSocket::new(net, symmetric_nat(), SocketAddr::from(([10, 9, 0, 2], 7777)));
        let (game, control) = relay_split(socket, relay_addr());
        let mut server = NetServer::with_secret(
            game,
            ServerConfig {
                crypto: CryptoMode::Required,
                max_peers: 40,
                // Kept at the default of 8: the relay IP is marked below, so 16 relayed clients
                // sharing it are counted by address.
                ..ServerConfig::default()
            },
            [6; 32],
        );
        server.add_relay_ip(relay_addr().ip());
        let ticket = JoinTicket::from_game_socket(server.server_key());
        let registrar = RelayRegistrar::new(Box::new(control), relay_addr(), &ticket).expect("ticket fits");
        Self { server, registrar }
    }
}

fn client_config(i: usize, key: Option<ServerKey>) -> ClientConfig {
    ClientConfig {
        name: format!("p{i}"),
        crypto: CryptoMode::Required,
        server_key: key,
        ..ClientConfig::default()
    }
}

/// Acceptance (N113): 16 clients join a host behind a symmetric NAT by join code; the direct
/// attempt cannot pass the host's NAT, so each falls back to its own relay allocation within
/// 1.5 s; all play sealed traffic both ways for 5 minutes on the relay path with no disconnect;
/// the relay holds 16 allocations and the host confirmed each. On the base the host registered
/// from a separate socket and no production code allocated, so a NAT'd host was unreachable.
#[test]
fn sixteen_clients_join_a_symmetric_nat_host_by_code_and_play_five_minutes_relayed() {
    let net = LoopbackNet::new();
    let mut relay = relay_v2(&net, (CLIENTS + 8) as u16);
    let mut host = Host::new(&net);
    let mut now = 0u64;
    let mut code = None;
    for _ in 0..50 {
        now += STEP_MS;
        if host.registrar.update(now) {
            code = host.registrar.join_code().map(str::to_owned);
        }
        relay.update(now);
        if code.is_some() {
            break;
        }
    }
    let code = code.expect("the host got a join code from its game socket");
    assert_eq!(relay.status().lobbies, 1);

    let mut paths = Vec::new();
    for i in 0..CLIENTS {
        // Each client behind its own symmetric NAT too: with NAT traversal (ADR-8340) an open
        // client would reach this host on a checked pair, and this test is about the relay path.
        let socket = NatSocket::new(
            &net,
            SimNat::new(
                NatMapping::AddressAndPortDependent,
                NatFiltering::AddressAndPortDependent,
                Ipv4Addr::new(203, 0, 113, i as u8 + 1),
            ),
            SocketAddr::from(([10, 1, 0, i as u8 + 1], 5000)),
        );
        let mut key = [0u8; 16];
        key[0] = i as u8;
        key[15] = 0xA7;
        paths.push(JoinPath::new(socket, relay_addr(), &code, key).expect("code"));
    }
    let mut clients: Vec<Option<NetClient<Duel, _>>> = (0..CLIENTS).map(|_| None).collect();
    let mut direct_since = vec![None; CLIENTS];
    let mut relayed_at = vec![None; CLIENTS];
    let start = now;
    let step =
        |now: u64,
         relay: &mut RelayServer<LoopbackSocket>,
         host: &mut Host,
         paths: &mut Vec<JoinPath<NatSocket>>,
         clients: &mut Vec<Option<NetClient<Duel, regia_net::server_host::JoinSocket<NatSocket>>>>| {
            for (path, client) in paths.iter_mut().zip(clients.iter_mut()) {
                path.update(now);
                if let Some(c) = client {
                    c.update(now);
                }
            }
            relay.update(now);
            host.registrar.update(now);
            host.server.update(now);
            relay.update(now);
        };
    // Join: every client resolves, races, and ends on its relay allocation.
    for _ in 0..1_000 {
        now += STEP_MS;
        for i in 0..CLIENTS {
            if clients[i].is_none()
                && let Some(ticket) = paths[i].ticket().cloned()
            {
                let socket = paths[i].take_socket().expect("the socket, once");
                assert_eq!(
                    ticket.server_key,
                    host.server.server_key(),
                    "the code carries the key"
                );
                clients[i] = Some(
                    NetClient::connect(
                        socket,
                        ticket.addr,
                        client_config(i, ticket.server_key),
                        &Team(i as u8 % 2),
                    )
                    .expect("join fits"),
                );
                direct_since[i] = Some(now);
            }
            if relayed_at[i].is_none() && matches!(paths[i].route(), JoinRoute::Relayed(_)) {
                relayed_at[i] = Some(now);
            }
        }
        step(now, &mut relay, &mut host, &mut paths, &mut clients);
        if clients
            .iter()
            .all(|c| c.as_ref().is_some_and(NetClient::is_connected))
        {
            break;
        }
    }
    for (i, path) in paths.iter().enumerate() {
        assert!(path.error().is_none(), "client {i}: {:?}", path.error());
        assert!(
            matches!(path.route(), JoinRoute::Relayed(_)),
            "client {i} is on the relay path, got {:?}",
            path.route()
        );
        assert_eq!(
            path.relay_protocol(),
            Some(regia_net::relay::RELAY_PROTOCOL_VERSION)
        );
        let (Some(at), Some(since)) = (relayed_at[i], direct_since[i]) else {
            panic!("client {i} never chose a path");
        };
        assert!(
            at - since <= RELAY_FALLBACK_MS + STEP_MS,
            "client {i} fell back to the relay {} ms after its direct attempt (bar 1.5 s)",
            at - since
        );
    }
    let connected = clients
        .iter()
        .filter(|c| c.as_ref().is_some_and(NetClient::is_connected))
        .count();
    assert_eq!(connected, CLIENTS, "every client joined through the relay");
    assert!(
        clients.iter().flatten().all(NetClient::is_sealed),
        "every relayed link is sealed"
    );
    assert_eq!(relay.status().allocations, CLIENTS, "one allocation per client");
    assert_eq!(host.registrar.relayed_clients(), CLIENTS as u64);
    assert_eq!(
        host.registrar.confirmed_allocations(),
        CLIENTS as u64,
        "the host confirmed each allocation from its game socket"
    );
    let peers: BTreeSet<SocketAddr> = host.server.session().peers().map(|p| p.addr).collect();
    assert_eq!(peers.len(), CLIENTS);
    assert!(
        peers.iter().all(|a| a.ip() == relay_addr().ip()),
        "every peer is a relay address"
    );
    assert_eq!(relay.core().allocations().len(), CLIENTS);

    // Play five minutes: each client sends a line every 200 ms, the host answers it.
    let play_steps = 5 * 60 * 1000 / STEP_MS;
    let mut heard = 0u64;
    let mut answers = vec![0u64; CLIENTS];
    let mut ended = Vec::new();
    for n in 0..play_steps {
        now += STEP_MS;
        if n % 10 == 0 {
            for (i, c) in clients.iter_mut().enumerate() {
                if let Some(c) = c {
                    c.send(Channel::GAME_RPC, &DuelMsg::Chat(format!("p{i} {n}")))
                        .expect("queued");
                }
            }
        }
        step(now, &mut relay, &mut host, &mut paths, &mut clients);
        while let Some(event) = host.server.poll_event() {
            match event {
                ServerEvent::Message {
                    peer,
                    message: DuelMsg::Chat(text),
                    ..
                } => {
                    heard += 1;
                    host.server
                        .send(peer, Channel::Unreliable, &DuelMsg::Chat(format!("ack {text}")))
                        .expect("queued");
                }
                ServerEvent::PeerLeft { peer, reason } => ended.push(format!("host: {peer:?} {reason:?}")),
                _ => {}
            }
        }
        for (i, c) in clients.iter_mut().enumerate() {
            let Some(c) = c else { continue };
            while let Some(event) = c.poll_event() {
                match event {
                    ClientEvent::Message { .. } => answers[i] += 1,
                    ClientEvent::Ended(end) => ended.push(format!("client {i}: {end:?}")),
                    _ => {}
                }
            }
        }
    }
    assert!(ended.is_empty(), "no disconnects in five minutes: {ended:?}");
    assert!(
        clients
            .iter()
            .all(|c| c.as_ref().is_some_and(NetClient::is_connected)),
        "every client still plays"
    );
    let sent_per_client = play_steps / 10;
    assert_eq!(
        heard,
        sent_per_client * CLIENTS as u64,
        "the host heard every reliable line"
    );
    assert!(
        answers.iter().all(|a| *a > sent_per_client / 2),
        "every client heard its answers: {answers:?}"
    );
    assert!(paths.iter().all(|p| matches!(p.route(), JoinRoute::Relayed(_))));
    let status = relay.status();
    assert_eq!(status.allocations, CLIENTS, "net.relay_status: 16 allocations");
    assert_eq!(
        status.permission_dropped, 0,
        "nothing reached a port without permission"
    );
    assert_eq!(status.forward_dropped + status.host_rate_dropped, 0);
    let views = relay.core().allocations();
    assert!(
        views.iter().all(|v| v.bytes_to_host > 0 && v.bytes_to_client > 0),
        "every allocation forwarded both ways: {views:?}"
    );
    assert!(
        host.registrar.join_code().is_some_and(|c| c == code),
        "the lobby kept its code"
    );
    let _ = start;
}

/// Acceptance (N113): a protocol 1 relay (no allocations) and a host it cannot reach directly:
/// the join ends with `relay_too_old` well inside the 5 s connect timeout, never a hang.
#[test]
fn a_protocol_1_relay_ends_the_join_with_relay_too_old_inside_the_connect_timeout() {
    let net = LoopbackNet::new();
    let mut relay = RelayV1::new(&net);
    let mut host = Host::new(&net);
    let mut now = 0u64;
    let mut code = None;
    for _ in 0..50 {
        now += STEP_MS;
        if host.registrar.update(now) {
            code = host.registrar.join_code().map(str::to_owned);
        }
        relay.update(now);
    }
    let code = code.expect("a protocol 1 relay still registers (protocol 1 framing)");
    let socket = net
        .bind(SocketAddr::from(([10, 1, 0, 1], 5000)))
        .expect("bind client");
    let mut path = JoinPath::new(socket, relay_addr(), &code, [7; 16]).expect("code");
    let mut client: Option<NetClient<Duel, _>> = None;
    let start = now;
    while path.error().is_none() && now - start < 10_000 {
        now += STEP_MS;
        path.update(now);
        if client.is_none()
            && let Some(ticket) = path.ticket().cloned()
        {
            let socket = path.take_socket().expect("socket");
            client = Some(
                NetClient::connect(socket, ticket.addr, client_config(0, ticket.server_key), &Team(0))
                    .expect("join fits"),
            );
        }
        if let Some(c) = &mut client {
            c.update(now);
        }
        relay.update(now);
        host.registrar.update(now);
        host.server.update(now);
    }
    assert_eq!(path.relay_protocol(), Some(1));
    assert_eq!(path.error().map(|e| e.code()), Some("relay_too_old"));
    assert_eq!(path.route(), JoinRoute::Failed);
    assert!(
        now - start < ClientConfig::default().connect_timeout_ms,
        "relay_too_old after {} ms, inside the connect timeout",
        now - start
    );
}

/// Acceptance (N113): `NetSession::connect(ConnectTarget::JoinCode)` joins end to end over real
/// UDP on 127.0.0.1: a host session on the game half of its socket, the registrar on the control
/// half, a relay in the process. On the base it returned an error.
#[test]
fn net_session_connects_by_join_code_end_to_end() {
    let localhost = SocketAddr::from(([127, 0, 0, 1], 0));
    let mut relay = regia_net::server_host::LocalRelay::bind(0, 31).expect("local relay");
    let relay_at = relay.addr();
    let game = UdpTransport::bind(localhost).expect("bind game port");
    let (game_half, control_half) = relay_split(game, relay_at);
    let mut server = NetSession::default();
    server.set_manual_clock(0);
    server
        .host_on(game_half, HostOptions::default(), false)
        .expect("host");
    assert!(server.add_relay_ip(relay_at.ip()));
    let ticket = JoinTicket::from_game_socket(server.server_key());
    let mut registrar = RelayRegistrar::new(Box::new(control_half), relay_at, &ticket).expect("fits");
    let mut now = 0;
    let mut code = None;
    for _ in 0..200 {
        now += STEP_MS;
        relay.update(now);
        if registrar.update(now) {
            code = registrar.join_code().map(str::to_owned);
        }
        relay.update(now);
        if code.is_some() {
            break;
        }
    }
    let code = code.expect("a join code");
    let mut client = NetSession::default();
    client.set_manual_clock(now);
    client
        .connect(
            ConnectTarget::JoinCode(format!("{code}@{relay_at}")),
            ConnectOptions::default(),
        )
        .expect("connect by join code");
    for _ in 0..1_000 {
        now += STEP_MS;
        client.set_manual_clock(now);
        server.set_manual_clock(now);
        client.update();
        relay.update(now);
        registrar.update(now);
        server.update();
        relay.update(now);
        if client.state() == ClientSessionState::InGame {
            break;
        }
    }
    assert_eq!(client.state(), ClientSessionState::InGame, "joined by code");
    assert_eq!(
        client.join_route(),
        Some(JoinRoute::DirectChosen),
        "on one machine the direct path wins"
    );
    assert!(
        client.server_key().is_some() && client.server_key() == server.server_key(),
        "the code pinned the key"
    );

    // A bare code without a relay is refused at once with a clear code.
    let mut lost = NetSession::default();
    let err = lost
        .connect(ConnectTarget::JoinCode(code.clone()), ConnectOptions::default())
        .expect_err("no relay");
    assert!(err.to_string().contains("join_relay_invalid"), "{err}");
    let _ = relay_version;
}

/// `NetSession` over a protocol 1 relay: the session ends with `relay_too_old` (the reason the
/// game shows), inside the session's connect timeout.
#[test]
fn net_session_join_through_a_protocol_1_relay_ends_with_relay_too_old() {
    let net = LoopbackNet::new();
    let mut relay = RelayV1::new(&net);
    let mut host = Host::new(&net);
    let mut now = 0u64;
    let mut code = None;
    for _ in 0..50 {
        now += STEP_MS;
        if host.registrar.update(now) {
            code = host.registrar.join_code().map(str::to_owned);
        }
        relay.update(now);
    }
    let code = code.expect("code");
    let mut client = NetSession::default();
    client.set_manual_clock(now);
    client
        .connect_by_code_on(
            net.bind(SocketAddr::from(([10, 1, 0, 9], 5000))).expect("bind"),
            relay_addr(),
            &code,
            ConnectOptions::default(),
        )
        .expect("connect");
    let start = now;
    let mut reason = None;
    while reason.is_none() && now - start < 20_000 {
        now += STEP_MS;
        client.set_manual_clock(now);
        client.update();
        relay.update(now);
        host.registrar.update(now);
        host.server.update(now);
        for e in client.take_client_events() {
            if let ClientSessionEvent::Disconnected { reason: r } = e {
                reason = Some(r);
            }
        }
    }
    assert_eq!(
        reason,
        Some(DisconnectReason::TransportError("relay_too_old".to_owned()))
    );
    assert!(
        now - start < 10_000,
        "ended after {} ms, inside the session's 10 s connect timeout",
        now - start
    );
}

/// A `NetSession` host behind the symmetric NAT and a `NetSession` client that joins by code:
/// the session plays on the relay path; after an Android-style suspend, `resume_on` joins by code
/// again (the path is chosen again) and the session is back in game with its grace slot.
#[test]
fn a_net_session_joins_a_nat_host_through_the_relay_and_resumes_by_code() {
    let net = LoopbackNet::new();
    let mut relay = relay_v2(&net, 8);
    let socket = NatSocket::new(&net, symmetric_nat(), SocketAddr::from(([10, 9, 0, 2], 7777)));
    let (game, control) = relay_split(socket, relay_addr());
    let mut server = NetSession::default();
    server.set_manual_clock(0);
    server.host_on(game, HostOptions::default(), false).expect("host");
    assert!(server.add_relay_ip(relay_addr().ip()));
    let mut registrar = RelayRegistrar::new(
        Box::new(control),
        relay_addr(),
        &JoinTicket::from_game_socket(server.server_key()),
    )
    .expect("fits");
    let mut now = 0u64;
    let mut code = None;
    for _ in 0..50 {
        now += STEP_MS;
        if registrar.update(now) {
            code = registrar.join_code().map(str::to_owned);
        }
        relay.update(now);
    }
    let code = code.expect("code");
    let mut client = NetSession::default();
    let mut drive = |client: &mut NetSession, now: &mut u64, until: &dyn Fn(&NetSession) -> bool| {
        for _ in 0..1_000 {
            *now += STEP_MS;
            client.set_manual_clock(*now);
            server.set_manual_clock(*now);
            client.update();
            relay.update(*now);
            registrar.update(*now);
            server.update();
            relay.update(*now);
            if until(client) {
                return true;
            }
        }
        false
    };
    client.set_manual_clock(now);
    // The client is behind a symmetric NAT too (ADR-8340: an open client would reach this host
    // on a checked pair; this test is about the relay path).
    let behind_nat = |ip: u8, port: u16| {
        NatSocket::new(
            &net,
            SimNat::new(
                NatMapping::AddressAndPortDependent,
                NatFiltering::AddressAndPortDependent,
                Ipv4Addr::new(203, 0, 113, ip),
            ),
            SocketAddr::from(([10, 1, 0, 9], port)),
        )
    };
    client
        .connect_by_code_on(
            behind_nat(90, 5000),
            relay_addr(),
            &code,
            ConnectOptions::default(),
        )
        .expect("connect");
    let in_game = |c: &NetSession| c.state() == ClientSessionState::InGame;
    assert!(
        drive(&mut client, &mut now, &in_game),
        "joined: {:?}",
        client.state()
    );
    assert!(
        matches!(client.join_route(), Some(JoinRoute::Relayed(_))),
        "{:?}",
        client.join_route()
    );
    assert!(client.link_sealed());

    client.suspend();
    let idle = |c: &NetSession| matches!(c.state(), ClientSessionState::Disconnected(_));
    assert!(
        drive(&mut client, &mut now, &idle),
        "suspended: {:?}",
        client.state()
    );
    let resumed = client.resume_on(behind_nat(91, 5001)).expect("resume");
    assert!(resumed, "a join by code resumes by code");
    assert_eq!(client.join_route(), Some(JoinRoute::Resolving));
    assert!(
        drive(&mut client, &mut now, &in_game),
        "back in game: {:?}",
        client.state()
    );
    assert!(matches!(client.join_route(), Some(JoinRoute::Relayed(_))));
    assert_eq!(
        relay.status().allocations,
        2,
        "the resumed client has an allocation of its own"
    );
}

/// N-19 (ADR-10069): the host withdraws its join code. A player already joined by the code
/// stays; the relay confirms; a later join by the same code is refused and never reaches the game.
#[test]
fn a_host_revokes_its_join_code_and_a_later_join_by_it_fails() {
    let localhost = SocketAddr::from(([127, 0, 0, 1], 0));
    let mut relay = regia_net::server_host::LocalRelay::bind(0, 37).expect("local relay");
    let relay_at = relay.addr();
    let game = UdpTransport::bind(localhost).expect("bind game port");
    let (game_half, control_half) = relay_split(game, relay_at);
    let mut server = NetSession::default();
    server.set_manual_clock(0);
    server
        .host_on(game_half, HostOptions::default(), false)
        .expect("host");
    assert!(server.add_relay_ip(relay_at.ip()));
    let ticket = JoinTicket::from_game_socket(server.server_key());
    let mut registrar = RelayRegistrar::new(Box::new(control_half), relay_at, &ticket).expect("fits");
    let mut now = 0;
    let mut code = None;
    for _ in 0..200 {
        now += STEP_MS;
        relay.update(now);
        if registrar.update(now) {
            code = registrar.join_code().map(str::to_owned);
        }
        relay.update(now);
        if code.is_some() {
            break;
        }
    }
    let code = code.expect("a join code");
    let join = |now: u64| {
        let mut c = NetSession::default();
        c.set_manual_clock(now);
        c.connect(
            ConnectTarget::JoinCode(format!("{code}@{relay_at}")),
            ConnectOptions::default(),
        )
        .expect("connect by join code");
        c
    };
    let mut early = join(now);
    for _ in 0..1_000 {
        now += STEP_MS;
        early.set_manual_clock(now);
        server.set_manual_clock(now);
        early.update();
        relay.update(now);
        registrar.update(now);
        server.update();
        relay.update(now);
        if early.state() == ClientSessionState::InGame {
            break;
        }
    }
    assert_eq!(early.state(), ClientSessionState::InGame, "joined by code");

    registrar.revoke_code(now);
    assert_eq!(
        registrar.join_code(),
        None,
        "the code is gone at once on the host"
    );
    for _ in 0..200 {
        now += STEP_MS;
        relay.update(now);
        registrar.update(now);
        relay.update(now);
        if registrar.code_revoked() {
            break;
        }
    }
    assert!(
        registrar.code_revoked(),
        "the relay never confirmed the withdrawal"
    );

    let mut late = join(now);
    for _ in 0..1_000 {
        now += STEP_MS;
        late.set_manual_clock(now);
        early.set_manual_clock(now);
        server.set_manual_clock(now);
        late.update();
        early.update();
        relay.update(now);
        registrar.update(now);
        server.update();
        relay.update(now);
        if matches!(late.state(), ClientSessionState::Disconnected(_)) {
            break;
        }
    }
    assert!(
        matches!(late.state(), ClientSessionState::Disconnected(_)),
        "a join by the withdrawn code: {:?}",
        late.state()
    );
    assert_eq!(
        early.state(),
        ClientSessionState::InGame,
        "the joined player stays"
    );
    assert_eq!(registrar.join_code(), None);
}
