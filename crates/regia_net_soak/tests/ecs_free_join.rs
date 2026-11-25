// SPDX-License-Identifier: MPL-2.0
//! A member's hub [`JoinPlan`] on a socket the game owns (ADR-10376).
//!
//! The hub, the relay and the owner's registrar run as in `lobby_hub_join`. The member does not
//! hand its game socket to [`HubReach`]. It keeps the [`SimNat`] socket and feeds
//! [`JoinPlanDriver`]: datagrams in, the driver's clock, datagrams out. The sealed handshake
//! completes with the owner's pinned [`ServerKey`] on the mapped, punch and relay paths.
//!
//! `regia_net`'s own tests pull dev-dependencies that turn `ecs` back on, so this target lives in
//! `regia_net_soak`, which depends on `regia_net` with `default-features = false` and enables
//! `regia_net/ecs` only through its own `ecs` feature.
//! `cargo test -p regia_net_soak --no-default-features --test ecs_free_join` is the build without
//! the ECS. Time is [`TestClock`] plus the hub's manual clock, stepped together. Nothing sleeps
//! and no assertion reads the wall clock.

use std::collections::HashMap;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use regia_net::crypto::{ServerIdentity, ServerKey};
use regia_net::lobby::hub::{
    HubClientConfig, HubClock, HubHost, HubLobbyBackend, JoinPlanDriver, NatClass, PlanEndpoint, ReachKind,
    ReachOffer, ReachState,
};
use regia_net::lobby::{LobbyBackend, LobbyEvent, LobbyId, LobbySpec};
use regia_net::nat::{Candidate, CandidateKind};
use regia_net::prelude::*;
use regia_net::relay::{RelayConfig, RelayCore, RelayServer, SimNat};
use regia_net::server_host::{
    HostClock, JoinTicket, RelayGamePort, RelayRegistrar, TestClock, fresh_allocation_key, relay_split,
};
use regia_net::socket::{LoopbackSocket, MAX_DATAGRAM_BYTES};
use regia_net::{ClientConfig, CryptoMode, ServerConfig};

/// Built with `--no-default-features`, `regia_net` has no ECS half. Referencing this fails the
/// build when that feature is off and `regia_net` still compiled its ECS half.
#[cfg(not(feature = "ecs"))]
const NO_ECS_IN_THIS_BUILD: () = assert!(!regia_net::ECS_FEATURE);

struct Duel;

impl NetGame for Duel {
    type Message = Vec<u8>;
    type Input = u32;
    type JoinData = u8;
    const GAME_ID: &'static str = "regia.test.duel";
    const PROTOCOL_VERSION: u32 = 1;
}

const STEP_MS: u64 = 10;
const RELAY_IP: [u8; 4] = [192, 0, 2, 1];
const HUB_ADDR: ([u8; 4], u16) = ([192, 0, 2, 9], 7810);

fn relay_addr() -> SocketAddr {
    SocketAddr::from((RELAY_IP, 7800))
}

fn hub_addr() -> SocketAddr {
    SocketAddr::from(HUB_ADDR)
}

/// A game socket on the in-memory network, behind a [`SimNat`] or open (a public address).
struct NatSocket {
    net: LoopbackNet,
    nat: Option<SimNat>,
    private: SocketAddr,
    open: Option<LoopbackSocket>,
    public: HashMap<u16, LoopbackSocket>,
}

impl NatSocket {
    fn new(net: &LoopbackNet, nat: Option<NatClass>, public_ip: Ipv4Addr, port: u16) -> Self {
        match nat {
            Some(c) => {
                let o = public_ip.octets();
                Self {
                    net: net.clone(),
                    nat: Some(SimNat::new(c.mapping, c.filtering, public_ip)),
                    private: SocketAddr::from(([10, o[2], o[3], 2], port)),
                    open: None,
                    public: HashMap::new(),
                }
            }
            None => {
                let a = SocketAddr::from((public_ip, port));
                Self {
                    net: net.clone(),
                    nat: None,
                    private: a,
                    open: Some(net.bind(a).expect("bind an open socket")),
                    public: HashMap::new(),
                }
            }
        }
    }
}

impl DatagramSocket for NatSocket {
    fn send_to(&mut self, bytes: &[u8], dest: SocketAddr) -> io::Result<()> {
        if let Some(open) = &mut self.open {
            return open.send_to(bytes, dest);
        }
        let Some(nat) = &mut self.nat else {
            return Ok(());
        };
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

    fn recv_from(&mut self, buf: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
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

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.private)
    }
}

type GameClient = NetClient<Duel, PlanEndpoint>;

struct Member {
    tag: &'static str,
    backend: HubLobbyBackend<LoopbackSocket>,
    events: Vec<LobbyEvent>,
    socket: Option<NatSocket>,
    driver: Option<JoinPlanDriver>,
    client: Option<GameClient>,
    reported: Option<(ReachKind, u32)>,
}

struct World {
    net: LoopbackNet,
    clock: HubClock,
    host_clock: TestClock,
    relay: RelayServer<LoopbackSocket>,
    hub: HubHost<LoopbackSocket>,
    hub_key: ServerKey,
    owner: HubLobbyBackend<LoopbackSocket>,
    owner_events: Vec<LobbyEvent>,
    game: NetServer<Duel, RelayGamePort<NatSocket>>,
    registrar: RelayRegistrar,
    lobby: LobbyId,
    members: Vec<Member>,
}

impl World {
    /// The owner's game socket is behind `host_nat` (`None`: a public address). `mapped` adds that
    /// address as a mapped candidate, which needs the socket to be open.
    fn new(host_nat: Option<NatClass>, mapped: bool) -> Self {
        let net = LoopbackNet::new();
        let control = net.bind(relay_addr()).expect("bind relay");
        let pool = (0..16u16)
            .map(|i| {
                net.bind(SocketAddr::from((RELAY_IP, 50_000 + i)))
                    .expect("bind allocation port")
            })
            .collect();
        let relay = RelayServer::from_core(
            control,
            pool,
            RelayCore::with_config(31, [9; 32], RelayConfig::default()),
        );
        let identity = ServerIdentity::from_private([0x41; 32]).expect("curve");
        let hub_key = identity.public();
        let hub = HubHost::new(
            net.bind(hub_addr()).expect("bind hub"),
            identity,
            regia_net::lobby::hub::HubSettings::default(),
        );
        let clock = HubClock::manual(0);
        let host_clock = TestClock::new();
        let game_socket = NatSocket::new(&net, host_nat, Ipv4Addr::new(198, 51, 100, 7), 7777);
        let (port, control_half) = relay_split(game_socket, relay_addr());
        let mut game = NetServer::with_secret(
            port,
            ServerConfig {
                crypto: CryptoMode::Required,
                max_peers: 16,
                ..ServerConfig::default()
            },
            [6; 32],
        );
        game.add_relay_ip(relay_addr().ip());
        let ticket = JoinTicket::from_game_socket(game.server_key());
        let registrar =
            RelayRegistrar::new(Box::new(control_half), relay_addr(), &ticket).expect("ticket fits");
        let owner = hub_client(&net, 6000, "host", hub_key, &clock, true);
        let mut w = Self {
            net,
            clock,
            host_clock,
            relay,
            hub,
            hub_key,
            owner,
            owner_events: Vec::new(),
            game,
            registrar,
            lobby: LobbyId(0),
            members: Vec::new(),
        };
        w.run("the owner gets a session and a join code", |w| {
            w.owner.local_member().is_some() && w.registrar.join_code().is_some()
        });
        w.owner.create(&LobbySpec::default()).expect("create");
        w.run("the owner's lobby exists", |w| {
            w.owner_events
                .iter()
                .any(|e| matches!(e, LobbyEvent::Created { result: Ok(_), .. }))
        });
        w.lobby = w
            .owner_events
            .iter()
            .find_map(|e| match e {
                LobbyEvent::Created { result: Ok(id), .. } => Some(*id),
                _ => None,
            })
            .expect("created");
        let mut candidates: Vec<Candidate> = w.registrar.candidates().to_vec();
        if mapped {
            candidates.push(Candidate::new(
                CandidateKind::Mapped,
                SocketAddr::from(([198, 51, 100, 7], 7777)),
            ));
        }
        let mut offer = ReachOffer::default();
        offer.candidates = candidates;
        offer.nat = host_nat;
        offer.relay = Some(relay_addr());
        offer.join_code = w.registrar.join_code().map(str::to_owned);
        offer.ticket = ticket.encode();
        w.owner.offer_reach(w.lobby, offer).expect("offer");
        w
    }

    fn add_member(&mut self, nat: Option<NatClass>) {
        let backend = hub_client(&self.net, 6001, "member", self.hub_key, &self.clock, false);
        self.members.push(Member {
            tag: "member",
            backend,
            events: Vec::new(),
            socket: Some(NatSocket::new(
                &self.net,
                nat,
                Ipv4Addr::new(203, 0, 113, 10),
                5000,
            )),
            driver: None,
            client: None,
            reported: None,
        });
        self.run("the member gets a session", |w| {
            w.members[0].backend.local_member().is_some()
        });
        let lobby = self.lobby;
        self.members[0].backend.join(lobby).expect("join");
        self.run("the member is in the lobby", |w| {
            w.members[0].backend.still_in(lobby)
        });
        self.members[0]
            .backend
            .ask_reach(lobby, {
                let mut offer = ReachOffer::default();
                offer.nat = nat;
                offer
            })
            .expect("ask");
    }

    fn step(&mut self) {
        self.host_clock.advance(Duration::from_millis(STEP_MS));
        let now = u64::try_from(self.host_clock.now().as_millis()).expect("step counter fits");
        self.clock.set_ms(now);
        self.owner_events.extend(self.owner.poll());
        self.relay.update(now);
        self.registrar.update(now);
        self.game.update(now);
        while self.game.poll_event().is_some() {}
        let lobby = self.lobby;
        for member in &mut self.members {
            pump_member(member, lobby, now);
        }
        self.hub.update(now);
        self.relay.update(now);
    }

    fn run(&mut self, what: &str, mut done: impl FnMut(&mut Self) -> bool) {
        for _ in 0..3000 {
            if done(self) {
                return;
            }
            self.step();
        }
        panic!("{what}: did not happen within 3000 steps");
    }

    fn reach(&mut self) {
        self.run("the member's sealed handshake completes", |w| {
            w.members.iter().all(|m| m.reported.is_some())
        });
        assert!(
            self.host_clock.now() >= Duration::from_millis(STEP_MS),
            "the join ran on TestClock"
        );
    }
}

fn pump_member(member: &mut Member, lobby: LobbyId, now: u64) {
    member.events.extend(member.backend.poll());
    if member.driver.is_none()
        && let Some(plan) = member.backend.take_plans().into_iter().next()
    {
        let local = member
            .socket
            .as_ref()
            .expect("the game socket")
            .local_addr()
            .expect("local address");
        member.driver =
            Some(JoinPlanDriver::start(local, &plan, fresh_allocation_key()).expect("a plan to run"));
    }
    let Member {
        tag,
        backend,
        events,
        socket: Some(socket),
        driver: Some(driver),
        client,
        reported,
    } = member
    else {
        return;
    };
    feed(socket, driver);
    if let ReachState::Failed(code) = driver.advance(now) {
        panic!("{tag}: the reach failed: {code}");
    }
    flush(socket, driver);
    if client.is_none()
        && let Some((endpoint, addr, key)) = driver.take_endpoint()
    {
        assert!(key.is_some(), "{tag}: the plan pins the owner's game key");
        let config = ClientConfig {
            name: (*tag).into(),
            crypto: CryptoMode::Required,
            server_key: key,
            ..ClientConfig::default()
        };
        *client = Some(NetClient::connect(endpoint, addr, config, &0u8).expect("join fits"));
    }
    if let Some(c) = client.as_mut() {
        c.update(now);
        if reported.is_none()
            && c.is_connected()
            && let Some(outcome) = driver.outcome()
        {
            backend.report_reach(lobby, outcome.0, outcome.1).expect("report");
            *reported = Some(outcome);
            // `report_reach` queues `LobbyEvent::Reached` on this backend. The poll at the top of
            // the step already ran, and the join stops as soon as `reported` is set.
            events.extend(backend.poll());
        }
    }
    flush(socket, driver);
}

fn feed(socket: &mut NatSocket, driver: &JoinPlanDriver) {
    let mut buf = [0u8; MAX_DATAGRAM_BYTES];
    loop {
        match socket.recv_from(&mut buf) {
            Ok(Some((n, from))) => {
                assert!(
                    driver.push_datagram(from, &buf[..n]),
                    "a datagram the socket delivered fits the plan queue"
                );
            }
            Ok(None) => break,
            Err(e) => panic!("member socket recv: {e}"),
        }
    }
}

fn flush(socket: &mut NatSocket, driver: &JoinPlanDriver) {
    while let Some((bytes, dest)) = driver.poll_send() {
        socket
            .send_to(&bytes, dest)
            .expect("the plan's datagram leaves on the game socket");
    }
}

fn hub_client(
    net: &LoopbackNet,
    port: u16,
    handle: &str,
    key: ServerKey,
    clock: &HubClock,
    auto_approve: bool,
) -> HubLobbyBackend<LoopbackSocket> {
    let socket = net.bind(SocketAddr::from(([192, 0, 2, 50], port))).expect("bind");
    HubLobbyBackend::connect(
        socket,
        hub_addr(),
        HubClientConfig {
            handle: handle.into(),
            server_key: Some(key),
            clock: clock.clone(),
            auto_approve,
            ..HubClientConfig::default()
        },
    )
    .expect("pinned")
}

fn sealed(w: &World) {
    let member = &w.members[0];
    let client = member.client.as_ref().expect("endpoint");
    assert!(client.is_connected(), "the sealed handshake completed");
    let pinned = client.server_key().expect("the client pinned a key");
    assert_eq!(
        Some(pinned),
        w.game.server_key(),
        "the pin is the owner's ServerKey"
    );
    assert_eq!(w.game.peer_rows().len(), 1);
    assert!(
        member
            .events
            .iter()
            .any(|e| matches!(e, LobbyEvent::Reached { .. })),
        "report_reach emits LobbyEvent::Reached"
    );
}

/// The owner's port mapping is dialed on the caller's socket. The relay allocates nothing.
#[test]
fn mapped_join_plan_on_a_caller_owned_socket() {
    let mut w = World::new(None, true);
    w.add_member(Some(NatClass::SYMMETRIC));
    w.reach();
    let member = &w.members[0];
    assert_eq!(member.reported, Some((ReachKind::Mapped, 0)));
    assert!(
        member.driver.as_ref().and_then(|d| d.join_path()).is_none(),
        "no join path ran"
    );
    assert_eq!(w.relay.status().allocations, 0);
    assert_eq!(
        member.driver.as_ref().map(|d| d.plan().path),
        Some(ReachKind::Mapped)
    );
    sealed(&w);
}

/// A full-cone member and a port-restricted host punch a direct pair on the caller's socket.
#[test]
fn punch_join_plan_on_a_caller_owned_socket() {
    let mut w = World::new(Some(NatClass::PORT_RESTRICTED), false);
    w.add_member(Some(NatClass::FULL_CONE));
    w.reach();
    let member = &w.members[0];
    assert_eq!(member.reported.map(|r| r.0), Some(ReachKind::Punch));
    assert!(
        member.reported.is_some_and(|r| r.1 > 0),
        "a punched path sent checks"
    );
    assert_eq!(
        member.driver.as_ref().map(|d| d.plan().path),
        Some(ReachKind::Punch)
    );
    sealed(&w);
}

/// Two symmetric NATs take the relay at once. The member's join path sends no checks, and the
/// relay allocation carries the sealed handshake.
#[test]
fn relay_join_plan_on_a_caller_owned_socket() {
    let mut w = World::new(Some(NatClass::SYMMETRIC), false);
    w.add_member(Some(NatClass::SYMMETRIC));
    w.reach();
    let member = &w.members[0];
    assert_eq!(member.reported, Some((ReachKind::Relay, 0)));
    let path = member
        .driver
        .as_ref()
        .and_then(|d| d.join_path())
        .expect("a join path");
    assert!(path.checks().is_none(), "no introduction, no checks");
    assert!(
        w.relay.status().allocations >= 1,
        "the relay allocation carries it"
    );
    assert_eq!(
        member.driver.as_ref().map(|d| d.plan().path),
        Some(ReachKind::Relay)
    );
    sealed(&w);
}

/// This target, built with `--no-default-features`, did not enable `ecs`.
#[cfg(not(feature = "ecs"))]
#[test]
fn ecs_feature_is_off() {
    let () = NO_ECS_IN_THIS_BUILD;
}
