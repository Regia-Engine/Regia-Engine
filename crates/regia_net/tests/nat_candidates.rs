// SPDX-License-Identifier: MPL-2.0
//! NAT traversal from the game socket (spec 120 § 10.7, ADR-8340, plan N228): a client joins a
//! host by code across every pair of NAT behaviors of `SimNat` (RFC 4787 mapping × filtering, plus
//! no NAT). The relay (protocol 3) introduces both sides with their candidates and a session key,
//! both run paced connectivity checks from their game sockets, and the sealed handshake runs on the
//! best answered pair; where no pair can work (symmetric on both sides) the relay pair wins at
//! 1.5 s. On the base nothing in production sent a punch introduction, so every pair the direct
//! address could not reach went through the relay.
//!
//! Time is a step counter on the in-memory network; every wait is a bounded loop on a condition.
//! Nothing sleeps and no assertion reads the wall clock.

mod common;

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use common::{Duel, DuelMsg, Team};
use regia_net::CryptoMode;
use regia_net::conditioner::{ConditionerConfig, NetConditioner};
use regia_net::nat::{NAT_CHECK_FLOOR_MS, NAT_CHECK_PACE_MS, is_check};
use regia_net::prelude::*;
use regia_net::relay::{
    NatFiltering, NatMapping, PunchResult, RELAY_PROTOCOL_VERSION, RelayConfig, RelayCore, RelayServer,
    SimNat, relay_version, simulate_hole_punch,
};
use regia_net::server_host::{
    JoinPath, JoinRoute, JoinSocket, JoinTicket, RELAY_FALLBACK_MS, RelayGamePort, RelayRegistrar,
    relay_split,
};
use regia_net::socket::LoopbackSocket;

const STEP_MS: u64 = 10;
const RELAY_IP: [u8; 4] = [192, 0, 2, 1];

fn relay_addr() -> SocketAddr {
    SocketAddr::from((RELAY_IP, 7800))
}

/// A NAT behavior, or none (a public address).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Behind {
    Open,
    Nat(NatMapping, NatFiltering),
}

impl Behind {
    fn all() -> Vec<Self> {
        let mut out = vec![Self::Open];
        for m in NatMapping::ALL {
            for f in NatFiltering::ALL {
                out.push(Self::Nat(m, f));
            }
        }
        out
    }

    fn sim(self, ip: Ipv4Addr) -> Option<SimNat> {
        match self {
            Self::Open => None,
            Self::Nat(m, f) => Some(SimNat::new(m, f, ip)),
        }
    }
}

const SYMMETRIC: Behind = Behind::Nat(
    NatMapping::AddressAndPortDependent,
    NatFiltering::AddressAndPortDependent,
);

/// Every connectivity check a socket sent: (time, destination, session key).
type CheckLog = Arc<Mutex<Vec<(u64, SocketAddr, [u8; 16])>>>;

/// A socket on the in-memory network, behind a [`SimNat`] or open. Each mapping the NAT makes is
/// a public socket on its public IP; inbound datagrams pass only where the filter lets them. With
/// a conditioner, both directions of its leg are impaired (seeded).
struct NatSocket {
    net: LoopbackNet,
    nat: Option<SimNat>,
    private: SocketAddr,
    open: Option<LoopbackSocket>,
    public: HashMap<u16, LoopbackSocket>,
    clock: Arc<AtomicU64>,
    checks: CheckLog,
    out: Option<NetConditioner>,
    inbound: Option<NetConditioner>,
    held_in: Vec<(Vec<u8>, SocketAddr)>,
    /// The private address on a shared LAN (10.0.0.0/8 traffic goes there, unfiltered).
    lan: Option<LoopbackSocket>,
}

impl NatSocket {
    fn new(
        net: &LoopbackNet,
        behind: Behind,
        public_ip: Ipv4Addr,
        port: u16,
        clock: &Arc<AtomicU64>,
    ) -> Self {
        let nat = behind.sim(public_ip);
        let (private, open) = match nat {
            Some(_) => {
                let o = public_ip.octets();
                (SocketAddr::from(([10, o[2], o[3], 2], port)), None)
            }
            None => {
                let a = SocketAddr::from((public_ip, port));
                (a, Some(net.bind(a).expect("bind open socket")))
            }
        };
        Self {
            net: net.clone(),
            nat,
            private,
            open,
            public: HashMap::new(),
            clock: clock.clone(),
            checks: Arc::default(),
            out: None,
            inbound: None,
            held_in: Vec::new(),
            lan: None,
        }
    }

    /// Puts the private side on a LAN shared with other `on_lan` sockets.
    fn on_lan(mut self) -> Self {
        self.lan = Some(self.net.bind(self.private).expect("bind the LAN side"));
        self
    }

    fn conditioned(mut self, config: &ConditionerConfig, seed: u64) -> Self {
        self.out = Some(NetConditioner::new(ConditionerConfig {
            seed,
            ..config.clone()
        }));
        self.inbound = Some(NetConditioner::new(ConditionerConfig {
            seed: seed ^ 0x5eed,
            ..config.clone()
        }));
        self
    }

    fn raw_send(&mut self, bytes: &[u8], dest: SocketAddr) -> io::Result<()> {
        if let Some(lan) = &mut self.lan
            && matches!(dest.ip(), std::net::IpAddr::V4(ip) if ip.octets()[0] == 10)
        {
            return lan.send_to(bytes, dest);
        }
        if let Some(open) = &mut self.open {
            return open.send_to(bytes, dest);
        }
        let Some(nat) = &mut self.nat else { return Ok(()) };
        let public = nat.outbound(dest);
        if !self.public.contains_key(&public.port()) {
            let socket = self.net.bind(public)?;
            self.public.insert(public.port(), socket);
        }
        match self.public.get_mut(&public.port()) {
            Some(socket) => socket.send_to(bytes, dest),
            None => Ok(()),
        }
    }

    fn raw_recv(&mut self, buf: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
        if let Some(lan) = &mut self.lan
            && let Some(got) = lan.recv_from(buf)?
        {
            return Ok(Some(got));
        }
        if let Some(open) = &mut self.open {
            return open.recv_from(buf);
        }
        let mut ports: Vec<u16> = self.public.keys().copied().collect();
        ports.sort_unstable();
        for port in ports {
            let Some(socket) = self.public.get_mut(&port) else {
                continue;
            };
            while let Some((n, from)) = socket.recv_from(buf)? {
                if self.nat.as_ref().is_some_and(|nat| nat.inbound(from, port)) {
                    return Ok(Some((n, from)));
                }
            }
        }
        Ok(None)
    }
}

impl DatagramSocket for NatSocket {
    fn send_to(&mut self, bytes: &[u8], dest: SocketAddr) -> io::Result<()> {
        let now = self.clock.load(Ordering::Relaxed);
        if is_check(bytes) {
            let mut key = [0u8; 16];
            key.copy_from_slice(&bytes[5..21]);
            if bytes[4] == 1 {
                self.checks.lock().expect("log").push((now, dest, key));
            }
        }
        let Some(out) = &mut self.out else {
            return self.raw_send(bytes, dest);
        };
        let mut due = out.release_due(now);
        due.extend(out.process(bytes.to_vec(), dest, now));
        for (b, d) in due {
            self.raw_send(&b, d)?;
        }
        Ok(())
    }

    fn recv_from(&mut self, buf: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
        let now = self.clock.load(Ordering::Relaxed);
        if self.inbound.is_none() {
            return self.raw_recv(buf);
        }
        if let Some(out) = &mut self.out {
            for (b, d) in out.release_due(now) {
                self.raw_send(&b, d)?;
            }
        }
        let mut tmp = vec![0u8; buf.len()];
        while let Some((n, from)) = self.raw_recv(&mut tmp)? {
            if let Some(inbound) = &mut self.inbound {
                // The "destination" of an inbound hold is its source.
                self.held_in.extend(inbound.process(tmp[..n].to_vec(), from, now));
            }
        }
        if let Some(inbound) = &mut self.inbound {
            self.held_in.extend(inbound.release_due(now));
        }
        if self.held_in.is_empty() {
            return Ok(None);
        }
        let (bytes, from) = self.held_in.remove(0);
        let n = bytes.len().min(buf.len());
        buf[..n].copy_from_slice(&bytes[..n]);
        Ok(Some((n, from)))
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.private)
    }
}

/// A protocol 3 relay on the in-memory network.
fn relay(net: &LoopbackNet, ports: u16) -> RelayServer<LoopbackSocket> {
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
        RelayCore::with_config(31, [9; 32], RelayConfig::default()),
    )
}

struct Host {
    server: NetServer<Duel, RelayGamePort<NatSocket>>,
    registrar: RelayRegistrar,
    checks: CheckLog,
}

impl Host {
    fn new(
        net: &LoopbackNet,
        behind: Behind,
        clock: &Arc<AtomicU64>,
        condition: Option<&ConditionerConfig>,
        lan: bool,
    ) -> Self {
        let mut socket = NatSocket::new(net, behind, Ipv4Addr::new(198, 51, 100, 7), 7777, clock);
        if lan {
            socket = socket.on_lan();
        }
        if let Some(c) = condition {
            socket = socket.conditioned(c, 77);
        }
        let checks = socket.checks.clone();
        let (game, control) = relay_split(socket, relay_addr());
        let mut server = NetServer::with_secret(
            game,
            ServerConfig {
                crypto: CryptoMode::Required,
                max_peers: 40,
                ..ServerConfig::default()
            },
            [6; 32],
        );
        server.add_relay_ip(relay_addr().ip());
        let ticket = JoinTicket::from_game_socket(server.server_key());
        let registrar = RelayRegistrar::new(Box::new(control), relay_addr(), &ticket).expect("ticket fits");
        Self {
            server,
            registrar,
            checks,
        }
    }
}

fn client_config(i: usize, key: Option<regia_net::crypto::ServerKey>) -> ClientConfig {
    ClientConfig {
        name: format!("p{i}"),
        crypto: CryptoMode::Required,
        server_key: key,
        ..ClientConfig::default()
    }
}

type Client = NetClient<Duel, JoinSocket<NatSocket>>;

/// One host, `behinds.len()` clients, one relay: the whole join, stepped.
struct Rig {
    clock: Arc<AtomicU64>,
    relay: RelayServer<LoopbackSocket>,
    host: Host,
    paths: Vec<JoinPath<NatSocket>>,
    clients: Vec<Option<Client>>,
    client_checks: Vec<CheckLog>,
    ticket_at: Vec<Option<u64>>,
    relayed_at: Vec<Option<u64>>,
    connected_at: Vec<Option<u64>>,
}

impl Rig {
    fn now(&self) -> u64 {
        self.clock.load(Ordering::Relaxed)
    }

    fn new(host_behind: Behind, behinds: &[Behind], condition: Option<&ConditionerConfig>) -> Self {
        Self::with(host_behind, behinds, condition, false)
    }

    /// `lan`: host and clients also share one LAN (their private addresses reach each other).
    fn with(
        host_behind: Behind,
        behinds: &[Behind],
        condition: Option<&ConditionerConfig>,
        lan: bool,
    ) -> Self {
        let net = LoopbackNet::new();
        let clock = Arc::new(AtomicU64::new(0));
        let mut relay = relay(&net, behinds.len() as u16 + 8);
        let mut host = Host::new(&net, host_behind, &clock, condition, lan);
        let mut code = None;
        for _ in 0..200 {
            let now = clock.fetch_add(STEP_MS, Ordering::Relaxed) + STEP_MS;
            if host.registrar.update(now) {
                code = host.registrar.join_code().map(str::to_owned);
            }
            relay.update(now);
            if code.is_some() {
                break;
            }
        }
        let code = code.expect("the host got a join code from its game socket");
        let mut paths = Vec::new();
        let mut client_checks = Vec::new();
        for (i, behind) in behinds.iter().enumerate() {
            let mut socket = NatSocket::new(
                &net,
                *behind,
                Ipv4Addr::new(203, 0, 113, i as u8 + 10),
                5000,
                &clock,
            );
            if lan {
                socket = socket.on_lan();
            }
            if let Some(c) = condition {
                socket = socket.conditioned(c, 1000 + i as u64);
            }
            client_checks.push(socket.checks.clone());
            let mut key = [0u8; 16];
            key[0] = i as u8;
            key[15] = 0x3C;
            paths.push(JoinPath::new(socket, relay_addr(), &code, key).expect("code"));
        }
        let n = behinds.len();
        Self {
            clock,
            relay,
            host,
            paths,
            clients: (0..n).map(|_| None).collect(),
            client_checks,
            ticket_at: vec![None; n],
            relayed_at: vec![None; n],
            connected_at: vec![None; n],
        }
    }

    fn step(&mut self) {
        let now = self.clock.fetch_add(STEP_MS, Ordering::Relaxed) + STEP_MS;
        for i in 0..self.paths.len() {
            self.paths[i].update(now);
            if self.clients[i].is_none()
                && let Some(ticket) = self.paths[i].ticket().cloned()
            {
                let socket = self.paths[i].take_socket().expect("the socket, once");
                assert_eq!(
                    ticket.server_key,
                    self.host.server.server_key(),
                    "the code carries the key"
                );
                self.clients[i] = Some(
                    NetClient::connect(
                        socket,
                        ticket.addr,
                        client_config(i, ticket.server_key),
                        &Team(i as u8 % 2),
                    )
                    .expect("join fits"),
                );
                self.ticket_at[i] = Some(now);
            }
            if let Some(c) = &mut self.clients[i] {
                c.update(now);
                if self.connected_at[i].is_none() && c.is_connected() {
                    self.connected_at[i] = Some(now);
                }
            }
            if self.relayed_at[i].is_none() && matches!(self.paths[i].route(), JoinRoute::Relayed(_)) {
                self.relayed_at[i] = Some(now);
            }
        }
        self.relay.update(now);
        self.host.registrar.update(now);
        self.host.server.update(now);
        while self.host.server.poll_event().is_some() {}
        self.relay.update(now);
    }

    /// Steps until every client is connected (or `limit_ms` of joining passed).
    fn join(&mut self, limit_ms: u64) {
        let end = self.now() + limit_ms;
        while self.now() < end {
            self.step();
            if self
                .clients
                .iter()
                .all(|c| c.as_ref().is_some_and(NetClient::is_connected))
            {
                break;
            }
        }
    }
}

/// What the RFC 4787 model says a pair can do without the relay (`simulate_hole_punch`); an open
/// side is reachable by anyone. One refinement: ICE takes a pair as valid only when the answer
/// comes back from the address the check went to (RFC 8445 § 7.2.5.2.1). When both sides map per
/// address and port, every answer leaves from a fresh port, so no pair is ever symmetric and the
/// relay carries the join, even where both filters are permissive enough for the model's
/// one-way deliveries.
fn predicted(host: Behind, client: Behind) -> PunchResult {
    if let (
        Behind::Nat(NatMapping::AddressAndPortDependent, _),
        Behind::Nat(NatMapping::AddressAndPortDependent, _),
    ) = (host, client)
    {
        return PunchResult::NeedsRelay;
    }
    match (
        host.sim(Ipv4Addr::new(198, 51, 100, 7)),
        client.sim(Ipv4Addr::new(203, 0, 113, 10)),
    ) {
        (Some(mut h), Some(mut c)) => simulate_hole_punch(&mut h, &mut c, relay_addr()),
        _ => PunchResult::Direct,
    }
}

/// Acceptance (N228): every pair of NAT behaviors (no NAT plus the nine RFC 4787 mapping ×
/// filtering combinations, host × client: 100 pairs) joins by code and finishes the sealed
/// handshake. Every pair the RFC 4787 model can punch goes direct on a checked pair (the relay is
/// never used), the others are relayed within 1.5 s of the introduction. On the base no production
/// code sent a punch introduction or a check, so a host behind an address-dependent mapping was
/// reachable only through the relay (and this file did not compile: no `nat` module).
#[test]
fn every_nat_type_pair_joins_by_code_direct_where_it_can_punch_and_relayed_where_it_cannot() {
    let mut results = BTreeMap::new();
    let mut failures = Vec::new();
    for host in Behind::all() {
        for client in Behind::all() {
            let mut rig = Rig::new(host, &[client], None);
            rig.join(6_000);
            let path = &rig.paths[0];
            let connected = rig.clients[0]
                .as_ref()
                .is_some_and(|c| c.is_connected() && c.is_sealed());
            let route = path.route();
            let expect = predicted(host, client);
            let ok = connected
                && path.error().is_none()
                && match expect {
                    PunchResult::Direct => {
                        route == JoinRoute::DirectChosen && rig.relay.status().allocations == 0
                    }
                    PunchResult::NeedsRelay => {
                        matches!(route, JoinRoute::Relayed(_))
                            && rig.relayed_at[0]
                                .zip(rig.ticket_at[0])
                                .is_some_and(|(r, t)| r - t <= RELAY_FALLBACK_MS + STEP_MS)
                    }
                };
            if !ok {
                failures.push(format!(
                    "host {host:?} client {client:?}: route {route:?} connected {connected} error {:?} predicted {expect:?} checks {:?}",
                    path.error(),
                    path.checks().map(|a| a.pairs().iter().map(|p| (p.remote.kind, p.remote.addr, p.state)).collect::<Vec<_>>())
                ));
            }
            *results.entry(format!("{expect:?}")).or_insert(0u32) += 1;
            assert_eq!(
                path.relay_protocol(),
                Some(RELAY_PROTOCOL_VERSION),
                "a protocol 3 relay"
            );
        }
    }
    assert!(
        failures.is_empty(),
        "{} of 100 pairs failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
    // The model's split: most pairs punch; only pairs with an address-dependent mapping facing a
    // filter they cannot open need the relay.
    assert!(results.get("Direct").copied().unwrap_or(0) > results.get("NeedsRelay").copied().unwrap_or(0));
    assert!(
        results.get("NeedsRelay").copied().unwrap_or(0) >= 1,
        "{results:?}"
    );
}

/// Acceptance (N228): symmetric NATs on both sides: no pair can work, the relay pair starts at
/// 500 ms and is nominated by 1.5 s after the introduction, and the sealed join completes on it.
#[test]
fn symmetric_on_both_sides_nominates_the_relay_within_1_5_s() {
    let mut rig = Rig::new(SYMMETRIC, &[SYMMETRIC], None);
    rig.join(6_000);
    let path = &rig.paths[0];
    assert!(path.error().is_none(), "{:?}", path.error());
    assert!(
        matches!(path.route(), JoinRoute::Relayed(_)),
        "{:?}",
        path.route()
    );
    let (Some(ticket), Some(relayed), Some(connected)) =
        (rig.ticket_at[0], rig.relayed_at[0], rig.connected_at[0])
    else {
        panic!(
            "never joined: {:?} {:?} {:?}",
            rig.ticket_at, rig.relayed_at, rig.connected_at
        );
    };
    assert!(
        relayed - ticket <= RELAY_FALLBACK_MS + STEP_MS,
        "relay nominated {} ms after the introduction (bar 1.5 s)",
        relayed - ticket
    );
    assert!(
        connected >= relayed,
        "the sealed handshake completed on the relay pair"
    );
    let agent = path.checks().expect("the relay introduced both sides");
    assert!(agent.best_valid().is_none(), "no direct pair answered");
    assert!(
        agent.counts().requests_sent >= 2,
        "the pairs were checked: {:?}",
        agent.counts()
    );
    assert_eq!(rig.relay.status().allocations, 1);
    assert_eq!(rig.relay.core().introductions, 1);
    let nat = rig.host.registrar.nat_report();
    assert_eq!(nat.sessions, 1);
    assert!(nat.checks_sent >= 1);
    assert_eq!(nat.nominated, 0);
}

/// Host candidates (spec 120 § 10.7): two players on one LAN, each behind a symmetric NAT (no
/// hairpinning): their reflexive addresses cannot reach each other, but the host candidates the
/// relay passed along do, and that pair has the highest priority, so the join goes direct over
/// the LAN instead of through the relay.
#[test]
fn two_symmetric_nat_peers_on_one_lan_go_direct_over_their_host_candidates() {
    let mut rig = Rig::with(SYMMETRIC, &[SYMMETRIC], None, true);
    rig.join(6_000);
    let path = &rig.paths[0];
    assert!(path.error().is_none(), "{:?}", path.error());
    assert_eq!(
        path.route(),
        JoinRoute::DirectChosen,
        "{:?}",
        path.checks().map(|a| a.pairs().to_vec())
    );
    assert!(
        rig.clients[0]
            .as_ref()
            .is_some_and(|c| c.is_connected() && c.is_sealed())
    );
    let host_private = SocketAddr::from(([10, 100, 7, 2], 7777));
    assert_eq!(
        path.direct_remote(),
        Some(host_private),
        "the LAN pair carries the game"
    );
    let best = path.checks().and_then(|a| a.best_valid()).expect("a valid pair");
    assert_eq!(best.remote.kind, regia_net::nat::CandidateKind::Host);
    assert_eq!(
        path.candidates().first().map(|c| c.addr),
        Some(SocketAddr::from(([10, 113, 10, 2], 5000))),
        "the client announced its own host candidate"
    );
    assert_eq!(rig.relay.status().allocations, 0, "the relay carried nothing");
    assert_eq!(
        rig.host.registrar.candidates().first().map(|c| c.addr),
        Some(host_private)
    );
    // The host learns the nomination from the client's nominating check.
    for _ in 0..50 {
        rig.step();
    }
    assert_eq!(rig.host.registrar.nat_report().nominated, 1);
}

/// Acceptance (N228): checks are paced. Sixteen clients behind port-restricted cones join at once:
/// each session (one client, and each of the host's sessions) sends at most one check per Ta
/// (50 ms, RFC 8445 § 14.2), and the host's sessions together never two within 5 ms.
#[test]
fn checks_are_paced_per_session_and_per_host() {
    let cone = Behind::Nat(
        NatMapping::EndpointIndependent,
        NatFiltering::AddressAndPortDependent,
    );
    let behinds = vec![cone; 16];
    let mut rig = Rig::new(SYMMETRIC, &behinds, None);
    rig.join(8_000);
    // Keep stepping so the sessions run to their end.
    for _ in 0..(12_000 / STEP_MS) {
        rig.step();
    }
    // One side's sends: virtual time, destination, session key. Aliased so the log's
    // type stays under clippy's type_complexity limit.
    type CheckSendLog = Vec<(u64, SocketAddr, [u8; 16])>;
    let mut logs: Vec<(String, CheckSendLog)> =
        vec![("host".into(), rig.host.checks.lock().expect("log").clone())];
    for (i, l) in rig.client_checks.iter().enumerate() {
        logs.push((format!("client {i}"), l.lock().expect("log").clone()));
    }
    let mut total = 0;
    for (who, log) in &logs {
        let mut by_session: BTreeMap<[u8; 16], Vec<u64>> = BTreeMap::new();
        for (t, _, key) in log {
            by_session.entry(*key).or_default().push(*t);
        }
        for times in by_session.values() {
            for w in times.windows(2) {
                assert!(
                    w[1] - w[0] >= NAT_CHECK_PACE_MS,
                    "{who}: two checks of one session {w:?} apart < Ta"
                );
            }
        }
        total += log.len();
    }
    let host_times: Vec<u64> = logs[0].1.iter().map(|(t, _, _)| *t).collect();
    for w in host_times.windows(2) {
        assert!(
            w[1] - w[0] >= NAT_CHECK_FLOOR_MS,
            "host: two checks {w:?} under the 5 ms floor"
        );
    }
    assert_eq!(rig.host.registrar.nat_report().sessions, 16);
    assert!(total > 16, "checks ran: {total}");
    for (i, c) in rig.clients.iter().enumerate() {
        assert!(
            c.as_ref().is_some_and(NetClient::is_connected),
            "client {i} joined"
        );
    }
}

/// The spec's `good-wifi` leg (spec 120 § 15.1, harness profile, ADR-7899): 4 ms plus up to 4 ms,
/// 0.2 % loss, 0.1 % reorders up to 10 ms, 0.05 % duplicates (the bandwidth cap is far above this
/// traffic and is not modeled here).
fn good_wifi() -> ConditionerConfig {
    ConditionerConfig {
        enabled: true,
        seed: 1,
        delay_ms: 4,
        jitter_ms: 4,
        loss: 0.002,
        reorder: 0.001,
        reorder_max_ms: 10,
        duplicate: 0.0005,
        ..ConditionerConfig::default()
    }
}

/// Acceptance (N228), profile relay-path (spec 120 § 15.1: every client through the relay,
/// good-wifi each leg): eight clients behind symmetric NATs join a symmetric host by code through
/// the candidate checks, fall back to the relay, and play five minutes with no disconnect.
#[test]
fn relay_path_profile_five_minutes_no_disconnect() {
    const CLIENTS: usize = 8;
    let wifi = good_wifi();
    let mut rig = Rig::new(SYMMETRIC, &[SYMMETRIC; CLIENTS], Some(&wifi));
    rig.join(8_000);
    for (i, p) in rig.paths.iter().enumerate() {
        assert!(
            matches!(p.route(), JoinRoute::Relayed(_)),
            "client {i}: {:?} {:?}",
            p.route(),
            p.error()
        );
        assert!(
            rig.clients[i].as_ref().is_some_and(NetClient::is_connected),
            "client {i} joined"
        );
        assert!(p.checks().is_some(), "client {i} was introduced");
    }
    let play_steps = 5 * 60 * 1000 / STEP_MS;
    let mut ended = Vec::new();
    let mut heard = 0u64;
    let mut answers = [0u64; CLIENTS];
    for n in 0..play_steps {
        if n % 20 == 0 {
            for (i, c) in rig.clients.iter_mut().enumerate() {
                if let Some(c) = c {
                    c.send(Channel::GAME_RPC, &DuelMsg::Chat(format!("p{i} {n}")))
                        .expect("queued");
                }
            }
        }
        let now = rig.clock.fetch_add(STEP_MS, Ordering::Relaxed) + STEP_MS;
        for (path, client) in rig.paths.iter_mut().zip(rig.clients.iter_mut()) {
            path.update(now);
            if let Some(c) = client {
                c.update(now);
            }
        }
        rig.relay.update(now);
        rig.host.registrar.update(now);
        rig.host.server.update(now);
        while let Some(event) = rig.host.server.poll_event() {
            match event {
                ServerEvent::Message {
                    peer,
                    message: DuelMsg::Chat(text),
                    ..
                } => {
                    heard += 1;
                    rig.host
                        .server
                        .send(peer, Channel::Unreliable, &DuelMsg::Chat(format!("ack {text}")))
                        .expect("queued");
                }
                ServerEvent::PeerLeft { peer, reason } => ended.push(format!("host: {peer:?} {reason:?}")),
                _ => {}
            }
        }
        rig.relay.update(now);
        for (i, c) in rig.clients.iter_mut().enumerate() {
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
    assert!(
        ended.is_empty(),
        "no disconnects in five minutes on relay-path: {ended:?}"
    );
    assert!(
        rig.clients
            .iter()
            .all(|c| c.as_ref().is_some_and(NetClient::is_connected))
    );
    let sent = (play_steps / 20) * CLIENTS as u64;
    // Reliable lines may still be in flight at the end (a lost datagram's resend).
    assert!(
        heard + CLIENTS as u64 * 4 >= sent,
        "the host heard {heard} of {sent} reliable lines"
    );
    assert!(
        answers.iter().all(|a| *a > (play_steps / 20) / 2),
        "answers: {answers:?}"
    );
    assert!(
        rig.paths
            .iter()
            .all(|p| matches!(p.route(), JoinRoute::Relayed(_)))
    );
    assert_eq!(rig.relay.status().allocations, CLIENTS);
}

/// A protocol 2 relay (it answers a protocol 3 request `VersionTooNew { max: 2 }`): the client goes
/// on with `Lookup` at once and joins as before (direct attempt, relay allocation), never a hang.
#[test]
fn a_protocol_2_relay_refuses_the_introduction_and_the_join_goes_on_with_lookup() {
    let net = LoopbackNet::new();
    let clock = Arc::new(AtomicU64::new(0));
    let mut relay = relay(&net, 8);
    let mut host = Host::new(&net, SYMMETRIC, &clock, None, false);
    let mut now = 0;
    let mut code = None;
    for _ in 0..100 {
        now += STEP_MS;
        clock.store(now, Ordering::Relaxed);
        if host.registrar.update(now) {
            code = host.registrar.join_code().map(str::to_owned);
        }
        relay.update(now);
    }
    let code = code.expect("registered");
    // The client's socket sits behind a shim that answers protocol 3 the way a protocol 2 relay
    // does, and passes everything else.
    let socket = NatSocket::new(&net, Behind::Open, Ipv4Addr::new(203, 0, 113, 50), 5000, &clock);
    let shim = Proto2Shim {
        inner: socket,
        answers: Vec::new(),
        refused: 0,
    };
    let mut path = JoinPath::new(shim, relay_addr(), &code, [1; 16]).expect("code");
    let mut client: Option<NetClient<Duel, JoinSocket<Proto2Shim>>> = None;
    for _ in 0..600 {
        now += STEP_MS;
        clock.store(now, Ordering::Relaxed);
        path.update(now);
        if client.is_none()
            && let Some(ticket) = path.ticket().cloned()
        {
            let socket = path.take_socket().expect("socket");
            client = Some(
                NetClient::connect(socket, ticket.addr, client_config(0, ticket.server_key), &Team(0))
                    .expect("fits"),
            );
        }
        if let Some(c) = &mut client {
            c.update(now);
        }
        relay.update(now);
        host.registrar.update(now);
        host.server.update(now);
        relay.update(now);
        if client.as_ref().is_some_and(NetClient::is_connected) {
            break;
        }
    }
    assert!(
        client.as_ref().is_some_and(NetClient::is_connected),
        "joined: {:?} {:?}",
        path.route(),
        path.error()
    );
    assert!(path.checks().is_none(), "a protocol 2 relay introduces nobody");
    assert_eq!(
        path.relay_protocol(),
        Some(2),
        "the HostInfo came in protocol 2 framing"
    );
    assert_eq!(relay.core().introductions, 0);
}

/// Answers a protocol 3 request to the relay `VersionTooNew { max: 2 }` (what a protocol 2 relay
/// sends) instead of letting it through; downgrades the relay's protocol 3 answers to protocol 2
/// framing so the client sees a protocol 2 relay.
struct Proto2Shim {
    inner: NatSocket,
    answers: Vec<(Vec<u8>, SocketAddr)>,
    refused: u32,
}

impl DatagramSocket for Proto2Shim {
    fn send_to(&mut self, bytes: &[u8], dest: SocketAddr) -> io::Result<()> {
        if dest == relay_addr() && relay_version(bytes) == Some(3) {
            self.refused += 1;
            let mut answer = b"RGNR".to_vec();
            answer.extend_from_slice(&[0, 2, 19, 2]);
            self.answers.push((answer, relay_addr()));
            return Ok(());
        }
        self.inner.send_to(bytes, dest)
    }

    fn recv_from(&mut self, buf: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
        if !self.answers.is_empty() {
            let (bytes, from) = self.answers.remove(0);
            buf[..bytes.len()].copy_from_slice(&bytes);
            return Ok(Some((bytes.len(), from)));
        }
        let got = self.inner.recv_from(buf)?;
        if let Some((n, from)) = got
            && from.ip() == relay_addr().ip()
            && n > 6
            && buf[..4] == *b"RGNR"
            && buf[4] == 0
            && buf[5] == 3
        {
            buf[5] = 2;
        }
        Ok(got)
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }
}
