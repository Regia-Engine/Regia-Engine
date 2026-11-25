// SPDX-License-Identifier: MPL-2.0
//! Join brokering through the self-hosted lobby hub (NL-05, ADR-8607), in process: a relay, a hub,
//! a lobby owner whose game server sits behind a simulated NAT (`relay::SimNat`, RFC 4787 mapping
//! × filtering), and members behind their own NATs. The owner offers its game socket's reach; each
//! member asks, the owner approves, the hub sends both sides the other's candidates and a plan in
//! the order mapped, punch, relay; the member runs the plan (`HubReach`) and its sealed game
//! handshake completes on the planned path.
//!
//! Player identity: every hub session is bound to the player key its sealed link proved. A forged
//! Hello (a proof for another link) and a forged Welcome (a hub naming another key) are refused.
//!
//! Time is a step counter on the in-memory network; every wait is a bounded loop on a condition.
//! Nothing sleeps and no assertion reads the wall clock.

#![cfg(feature = "ecs")]

mod common;

use std::collections::HashMap;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};

use common::{Duel, Team};
use regia_net::crypto::{PlayerIdentity, ServerIdentity, ServerKey};
use regia_net::lobby::hub::{
    ChatScope, HubClientConfig, HubClock, HubError, HubEvent, HubHost, HubLinkState, HubLobbyBackend,
    HubMessage, HubReach, HubRequest, HubServer, HubSettings, HubWire, JoinPlan, NatClass, ReachKind,
    ReachOffer, ReachSocket, ReachState,
};
use regia_net::lobby::{LobbyBackend, LobbyEvent, LobbyId, LobbySpec, MemberId};
use regia_net::nat::{Candidate, CandidateKind};
use regia_net::prelude::*;
use regia_net::relay::{NatFiltering, NatMapping, RelayConfig, RelayCore, RelayServer, SimNat};
use regia_net::server_host::{JoinTicket, RelayGamePort, RelayRegistrar, fresh_allocation_key, relay_split};
use regia_net::socket::LoopbackSocket;
use regia_net::{ClientConfig, CryptoMode, ServerConfig};

const STEP_MS: u64 = 10;
const RELAY_IP: [u8; 4] = [192, 0, 2, 1];
const HUB_ADDR: ([u8; 4], u16) = ([192, 0, 2, 9], 7810);

fn relay_addr() -> SocketAddr {
    SocketAddr::from((RELAY_IP, 7800))
}

fn hub_addr() -> SocketAddr {
    SocketAddr::from(HUB_ADDR)
}

/// A game socket on the in-memory network, behind a [`SimNat`] or open (a public address). Each
/// mapping the NAT makes is a public socket on its public IP; inbound datagrams pass only where
/// the filter lets them.
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

type GameClient = NetClient<Duel, ReachSocket<NatSocket>>;

/// One member: its hub client, its NAT, its plan and its game endpoint.
struct Member {
    tag: &'static str,
    backend: HubLobbyBackend<LoopbackSocket>,
    events: Vec<LobbyEvent>,
    socket: Option<NatSocket>,
    reach: Option<HubReach<NatSocket>>,
    client: Option<GameClient>,
    reported: Option<(ReachKind, u32)>,
}

/// The owner's side: hub client, game server behind its NAT, relay registrar.
struct World {
    net: LoopbackNet,
    clock: HubClock,
    now: u64,
    relay: RelayServer<LoopbackSocket>,
    hub: HubHost<LoopbackSocket>,
    hub_key: ServerKey,
    owner: HubLobbyBackend<LoopbackSocket>,
    owner_events: Vec<LobbyEvent>,
    game: NetServer<Duel, RelayGamePort<NatSocket>>,
    registrar: RelayRegistrar,
    lobby: LobbyId,
    members: Vec<Member>,
    next_hub_port: u16,
}

impl World {
    /// A world whose owner's game socket is behind `host_nat` (`None`: a public address);
    /// `mapped` adds a port mapping of it as a `mapped` candidate (the game socket must be open).
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
            HubSettings::default(),
        );
        let clock = HubClock::manual(0);
        let game_socket = NatSocket::new(&net, host_nat, Ipv4Addr::new(198, 51, 100, 7), 7777);
        let (port, control) = relay_split(game_socket, relay_addr());
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
        let registrar = RelayRegistrar::new(Box::new(control), relay_addr(), &ticket).expect("ticket fits");
        let owner = hub_client(&net, 6000, "host", hub_key, &clock, true, None);
        let mut w = Self {
            net,
            clock,
            now: 0,
            relay,
            hub,
            hub_key,
            owner,
            owner_events: Vec::new(),
            game,
            registrar,
            lobby: LobbyId(0),
            members: Vec::new(),
            next_hub_port: 6001,
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
        offer.relay_only = false;
        w.owner.offer_reach(w.lobby, offer).expect("offer");
        w
    }

    /// A member tagged `tag` (its platform) behind `nat` that tells the hub so: connects to the
    /// hub, joins the lobby and asks for a path.
    fn add_member(&mut self, tag: &'static str, nat: Option<NatClass>) {
        self.add_member_telling(tag, nat, nat);
    }

    /// As [`World::add_member`], behind `nat`, telling the hub `told`.
    fn add_member_telling(&mut self, tag: &'static str, nat: Option<NatClass>, told: Option<NatClass>) {
        let i = self.members.len() as u8;
        let backend = hub_client(
            &self.net,
            self.next_hub_port,
            tag,
            self.hub_key,
            &self.clock,
            false,
            None,
        );
        self.next_hub_port += 1;
        self.members.push(Member {
            tag,
            backend,
            events: Vec::new(),
            socket: Some(NatSocket::new(
                &self.net,
                nat,
                Ipv4Addr::new(203, 0, 113, 10 + i),
                5000,
            )),
            reach: None,
            client: None,
            reported: None,
        });
        let idx = self.members.len() - 1;
        self.run("the member gets a session", |w| {
            w.members[idx].backend.local_member().is_some()
        });
        let lobby = self.lobby;
        self.members[idx].backend.join(lobby).expect("join");
        self.run("the member is in the lobby", |w| {
            w.members[idx].backend.still_in(lobby)
        });
        let m = &self.members[idx];
        m.backend
            .set_member_data(lobby, "platform", Some(tag))
            .expect("platform tag");
        m.backend
            .ask_reach(lobby, {
                let mut offer = ReachOffer::default();
                offer.nat = told;
                offer
            })
            .expect("ask");
    }

    fn step(&mut self) {
        self.now += STEP_MS;
        let now = self.now;
        self.clock.set_ms(now);
        self.owner_events.extend(self.owner.poll());
        self.relay.update(now);
        self.registrar.update(now);
        self.game.update(now);
        while self.game.poll_event().is_some() {}
        for m in &mut self.members {
            m.events.extend(m.backend.poll());
            if m.reach.is_none()
                && let Some(plan) = m.backend.take_plans().into_iter().next()
            {
                let socket = m.socket.take().expect("the game socket, once");
                m.reach = Some(HubReach::new(socket, &plan, fresh_allocation_key()).expect("a plan to run"));
            }
            if let Some(reach) = &mut m.reach {
                if let ReachState::Failed(code) = reach.update(now) {
                    panic!("{}: the reach failed: {code}", m.tag);
                }
                if m.client.is_none()
                    && let Some((socket, addr, key)) = reach.take_socket()
                {
                    assert!(key.is_some(), "the plan pins the owner's game key");
                    let config = ClientConfig {
                        name: m.tag.into(),
                        crypto: CryptoMode::Required,
                        server_key: key,
                        ..ClientConfig::default()
                    };
                    m.client = Some(NetClient::connect(socket, addr, config, &Team(0)).expect("join fits"));
                }
            }
            if let Some(c) = &mut m.client {
                c.update(now);
                if m.reported.is_none()
                    && c.is_connected()
                    && let Some(outcome) = m.reach.as_ref().and_then(HubReach::outcome)
                {
                    m.backend
                        .report_reach(self.lobby, outcome.0, outcome.1)
                        .expect("report");
                    m.reported = Some(outcome);
                }
            }
        }
        self.hub.update(now);
        self.relay.update(now);
    }

    /// Steps until `done` holds; fails after 3000 steps (30 s of test clock).
    fn run(&mut self, what: &str, mut done: impl FnMut(&mut Self) -> bool) {
        for _ in 0..3000 {
            if done(self) {
                return;
            }
            self.step();
        }
        panic!("{what}: did not happen within 3000 steps");
    }

    fn reach_all(&mut self) {
        self.run("every member reaches the owner's game session", |w| {
            w.members.iter().all(|m| m.reported.is_some())
                && w.owner_events
                    .iter()
                    .filter(|e| matches!(e, LobbyEvent::Reached { .. }))
                    .count()
                    >= w.members.len()
        });
    }

    fn member(&self, tag: &str) -> &Member {
        self.members.iter().find(|m| m.tag == tag).expect("member")
    }
}

fn hub_client(
    net: &LoopbackNet,
    port: u16,
    handle: &str,
    key: ServerKey,
    clock: &HubClock,
    auto_approve: bool,
    identity: Option<PlayerIdentity>,
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
            identity,
            ..HubClientConfig::default()
        },
    )
    .expect("pinned")
}

const FULL_CONE: NatClass = NatClass::FULL_CONE;
const ADDRESS_RESTRICTED: NatClass = NatClass {
    mapping: NatMapping::EndpointIndependent,
    filtering: NatFiltering::AddressDependent,
};

/// Acceptance (NL-05): three members tagged android, deck and windows, behind a symmetric NAT, a
/// full cone and an address-restricted cone, join one lobby through the hub and reach the owner's
/// game server behind a port-restricted cone: the symmetric one over the relay (planned at once,
/// no checks), the cones over punched direct pairs. The hub reports each member's path, punch
/// attempts and relay use. On the base there was no join broker.
#[test]
fn android_deck_windows_join_one_lobby_through_the_hub() {
    let mut w = World::new(Some(NatClass::PORT_RESTRICTED), false);
    w.add_member("android", Some(NatClass::SYMMETRIC));
    w.add_member("deck", Some(FULL_CONE));
    w.add_member("windows", Some(ADDRESS_RESTRICTED));
    w.reach_all();

    assert_eq!(
        w.game.peer_rows().len(),
        3,
        "all three reached the owner's game server"
    );
    let expect = [
        ("android", ReachKind::Relay),
        ("deck", ReachKind::Punch),
        ("windows", ReachKind::Punch),
    ];
    for (tag, path) in expect {
        let m = w.member(tag);
        assert_eq!(m.reported.map(|r| r.0), Some(path), "{tag}");
        assert!(m.client.as_ref().is_some_and(NetClient::is_connected), "{tag}");
        assert!(
            m.events
                .iter()
                .any(|e| matches!(e, LobbyEvent::Reached { path: p, .. } if *p == path)),
            "{tag} sees its own path as a LobbyEvent"
        );
    }
    assert_eq!(
        w.member("android").reported.map(|r| r.1),
        Some(0),
        "a relay plan sends no checks"
    );
    assert!(
        w.member("deck").reported.is_some_and(|r| r.1 > 0),
        "a punched path sent checks"
    );
    let report = w.hub.report();
    assert_eq!(report.joins.len(), 3, "{:?}", report.joins);
    for row in &report.joins {
        assert_eq!(row.state, "reached", "{row:?}");
        assert_eq!(row.platform.as_deref(), Some(row.handle.as_str()), "{row:?}");
        assert_eq!(row.relayed, row.handle == "android", "{row:?}");
        assert_eq!(row.planned, row.chosen, "{row:?}");
        assert_eq!(row.identity.len(), 8, "{row:?}");
    }
    assert_eq!(report.joins[0].nat.as_deref(), Some("symmetric"));
    let owner_saw: Vec<(MemberId, ReachKind)> = w
        .owner_events
        .iter()
        .filter_map(|e| match e {
            LobbyEvent::Reached { member, path, .. } => Some((*member, *path)),
            _ => None,
        })
        .collect();
    assert_eq!(owner_saw.len(), 3, "the owner hears every member's path");
    // Every member's identity is its own key, as its hub transport proved it.
    for m in &w.members {
        let member = m.backend.local_member().expect("session");
        assert_eq!(
            w.hub.core().identity(member),
            Some(m.backend.identity()),
            "{}",
            m.tag
        );
    }
}

/// Acceptance (NL-05): a symmetric member behind a symmetric owner goes to the relay: the broker
/// plans it at once, the join path makes no checks, and the relay carries the session.
#[test]
fn symmetric_nat_falls_back_to_the_relay() {
    let mut w = World::new(Some(NatClass::SYMMETRIC), false);
    w.add_member("android", Some(NatClass::SYMMETRIC));
    w.reach_all();
    let m = w.member("android");
    assert_eq!(m.reported, Some((ReachKind::Relay, 0)));
    let path = m
        .reach
        .as_ref()
        .and_then(HubReach::join_path)
        .expect("a join path");
    assert!(path.checks().is_none(), "no introduction, no checks");
    assert_eq!(w.relay.status().allocations, 1, "the relay allocation carries it");
    assert!(w.hub.report().joins[0].relayed);

    // A symmetric member that does not tell the hub its NAT: the broker plans a punch, no pair
    // works, and the relay wins at 1.5 s.
    let mut w = World::new(Some(NatClass::SYMMETRIC), false);
    w.add_member_telling("unknown", Some(NatClass::SYMMETRIC), None);
    w.reach_all();
    let m = w.member("unknown");
    assert_eq!(m.reported.map(|r| r.0), Some(ReachKind::Relay));
    assert_eq!(w.hub.report().joins[0].planned, Some(ReachKind::Punch));
}

/// Acceptance (NL-05): the owner's port mapping beats punching and the relay, even for a
/// symmetric member: the plan is the mapped address, the member dials it directly, the relay
/// allocates nothing.
#[test]
fn mapped_path_beats_punch_and_relay() {
    let mut w = World::new(None, true);
    w.add_member("android", Some(NatClass::SYMMETRIC));
    w.reach_all();
    let m = w.member("android");
    assert_eq!(m.reported, Some((ReachKind::Mapped, 0)));
    assert!(
        m.reach.as_ref().and_then(HubReach::join_path).is_none(),
        "no join path ran"
    );
    assert_eq!(w.relay.status().allocations, 0);
    assert_eq!(w.game.peer_rows().len(), 1);
}

/// The owner approves each join: no plan before an offer and an ask, only the owner approves,
/// one plan per ask, and a denied member is told (`hub_reach_denied`) and planned nothing.
#[test]
fn the_owner_approves_each_join() {
    let mut hub = HubServer::with_secret(HubSettings::default(), [1; 32]);
    let key = |b: u8| PlayerIdentity::from_private([b; 32]).expect("curve").public();
    let owner = hub.connect_player(key(1), "o", 0).expect("o");
    let bob = hub.connect_player(key(2), "b", 0).expect("b");
    let cy = hub.connect_player(key(3), "c", 0).expect("c");
    let lobby = hub.create(owner, &LobbySpec::default(), 0).expect("lobby");
    hub.join(bob, lobby, None, 0).expect("join");
    hub.join(cy, lobby, None, 0).expect("join");
    assert_eq!(
        hub.ask_reach(bob, lobby, ReachOffer::default(), 0),
        Err(HubError::NoReach)
    );
    hub.offer_reach(
        owner,
        lobby,
        {
            let mut offer = ReachOffer::default();
            offer.relay = Some(relay_addr());
            offer.join_code = Some("abc-def".into());
            offer
        },
        0,
    )
    .expect("offer");
    assert_eq!(
        hub.offer_reach(
            owner,
            lobby,
            {
                let mut offer = ReachOffer::default();
                offer.join_code = Some("not a code".into());
                offer
            },
            0
        ),
        Err(HubError::ReachInvalid)
    );
    hub.ask_reach(bob, lobby, ReachOffer::default(), 0).expect("ask");
    hub.ask_reach(cy, lobby, ReachOffer::default(), 0).expect("ask");
    let asks: Vec<MemberId> = hub
        .take_events(owner)
        .iter()
        .filter_map(|e| match e {
            HubEvent::ReachAsk { member, identity, .. } => {
                assert_eq!(Some(*identity), hub.identity(*member));
                Some(*member)
            }
            _ => None,
        })
        .collect();
    assert_eq!(asks, [bob, cy]);
    let _ = hub.take_events(bob);
    let _ = hub.take_events(cy);
    assert_eq!(
        hub.approve(bob, lobby, cy, true, 0),
        Err(HubError::Lobby(regia_net::lobby::LobbyError::NotOwner))
    );
    let plan: JoinPlan = hub
        .approve(owner, lobby, bob, true, 0)
        .expect("approve")
        .expect("plan");
    assert_eq!(plan.path, ReachKind::Punch);
    assert_eq!(plan.peer, owner);
    assert_eq!(plan.peer_identity, key(1));
    assert_eq!(plan.join_code.as_deref(), Some("ABCDEF"), "stored normalized");
    assert!(matches!(&hub.take_events(bob)[..], [HubEvent::Plan(p)] if *p == plan));
    assert!(
        hub.take_events(owner)
            .iter()
            .any(|e| matches!(e, HubEvent::Plan(p) if p.peer == bob && p.peer_identity == key(2))),
        "the owner gets the member's side of the plan"
    );
    assert_eq!(
        hub.approve(owner, lobby, bob, true, 0),
        Err(HubError::NoAsk),
        "one plan per ask"
    );
    assert_eq!(hub.approve(owner, lobby, cy, false, 0), Ok(None));
    assert!(matches!(&hub.take_events(cy)[..], [HubEvent::ReachDenied { .. }]));
    assert_eq!(
        hub.reach_done(cy, lobby, ReachKind::Relay, 0, 0),
        Err(HubError::NoAsk),
        "a denied member has no path to report"
    );
    let states: Vec<String> = hub.joins().into_iter().map(|r| r.state).collect();
    assert_eq!(states, ["planned", "denied"]);
}

/// Acceptance (NL-05): a forged Welcome is refused. A hub that holds the pinned key but binds the
/// session to another player's key: the client ends its link with `hub_identity_mismatch` and has
/// no session. And a forged Hello: a proof made for another link (or another hub) is refused with
/// the same code, and the hub allocates nothing.
#[test]
fn a_forged_welcome_is_refused() {
    let net = LoopbackNet::new();
    let identity = ServerIdentity::from_private([0x42; 32]).expect("curve");
    let key = identity.public();
    // A rogue hub on the pinned key.
    let mut rogue: NetServer<HubWire, LoopbackSocket> = NetServer::new(
        net.bind(hub_addr()).expect("bind"),
        ServerConfig {
            identity: Some(identity),
            crypto: CryptoMode::Required,
            ..ServerConfig::default()
        },
    );
    let clock = HubClock::manual(0);
    let victim = PlayerIdentity::from_private([0x07; 32]).expect("curve");
    let other = PlayerIdentity::from_private([0x08; 32]).expect("curve");
    let client = hub_client(&net, 6100, "ann", key, &clock, false, Some(victim.clone()));
    let mut now = 0;
    let mut welcomed = false;
    for _ in 0..500 {
        now += STEP_MS;
        clock.set_ms(now);
        let _ = client.poll();
        rogue.update(now);
        while let Some(event) = rogue.poll_event() {
            if let ServerEvent::Message {
                peer,
                message: HubMessage::Request(HubRequest::Hello { .. }),
                ..
            } = event
            {
                let forged = HubEvent::Welcome {
                    member: MemberId(1),
                    handle: "ann".into(),
                    idle_ms: 30_000,
                    identity: other.public(),
                };
                rogue
                    .send(peer, Channel::GAME_RPC, &HubMessage::Event(forged))
                    .expect("send");
                welcomed = true;
            }
        }
        rogue.flush(now);
        if matches!(client.link_state(), HubLinkState::Ended(_)) {
            break;
        }
    }
    assert!(welcomed, "the rogue hub got a hello");
    assert_eq!(
        client.link_state(),
        HubLinkState::Ended("hub_identity_mismatch".into())
    );
    assert_eq!(client.local_member(), None);
    assert_eq!(
        client.create(&LobbySpec::default()),
        Err(regia_net::lobby::LobbyError::Unavailable)
    );

    // A forged Hello to the real hub: a raw endpoint of the hub's game whose proof is bound to
    // another link's handshake hash.
    let net = LoopbackNet::new();
    let identity = ServerIdentity::from_private([0x43; 32]).expect("curve");
    let key = identity.public();
    let mut hub = HubHost::new(
        net.bind(hub_addr()).expect("bind"),
        identity,
        HubSettings::default(),
    );
    let mut raw: NetClient<HubWire, LoopbackSocket> = NetClient::connect(
        net.bind(SocketAddr::from(([192, 0, 2, 60], 6200))).expect("bind"),
        hub_addr(),
        ClientConfig {
            crypto: CryptoMode::Required,
            server_key: Some(key),
            ..ClientConfig::default()
        },
        &(),
    )
    .expect("fits");
    let mut now = 0;
    while !raw.is_connected() {
        now += STEP_MS;
        raw.update(now);
        hub.update(now);
        assert!(now < 5_000, "the handshake did not finish");
    }
    let real_binding = raw.handshake_hash().expect("sealed");
    let mut forged_binding = real_binding;
    forged_binding[0] ^= 1;
    let attempts = [
        victim.prove(&key, &forged_binding).expect("proof"),
        victim
            .prove(
                &ServerIdentity::from_private([0x44; 32]).expect("curve").public(),
                &real_binding,
            )
            .expect("proof"),
        vec![0u8; regia_net::crypto::PLAYER_PROOF_BYTES],
    ];
    let mut refusals = 0;
    for proof in attempts {
        raw.send(
            Channel::GAME_RPC,
            &HubMessage::Request(HubRequest::Hello {
                handle: "ann".into(),
                proof,
            }),
        )
        .expect("send");
        for _ in 0..20 {
            now += STEP_MS;
            raw.update(now);
            hub.update(now);
            while let Some(e) = raw.poll_event() {
                if let ClientEvent::Message {
                    message: HubMessage::Event(HubEvent::Refused { code }),
                    ..
                } = e
                {
                    assert_eq!(code, "hub_identity_mismatch");
                    refusals += 1;
                }
            }
        }
    }
    assert_eq!(refusals, 3, "each forged proof is refused");
    assert!(hub.core().players().is_empty(), "no session for a forged hello");
    assert_eq!(hub.report().refused.get("hub_identity_mismatch"), Some(&3));
    // The real proof on this link is accepted, under the victim's key.
    raw.send(
        Channel::GAME_RPC,
        &HubMessage::Request(HubRequest::Hello {
            handle: "ann".into(),
            proof: victim.prove(&key, &real_binding).expect("proof"),
        }),
    )
    .expect("send");
    for _ in 0..20 {
        now += STEP_MS;
        raw.update(now);
        hub.update(now);
    }
    let ann = hub.core().member_by_handle("ann").expect("a session");
    assert_eq!(hub.core().identity(ann), Some(victim.public()));
}

/// One player key, one handle: the same key under another handle is refused
/// (`hub_identity_in_use`); under the same handle it gets its session back (a reconnect).
#[test]
fn a_player_key_keeps_its_handle_and_its_session() {
    let mut hub = HubServer::with_secret(HubSettings::default(), [1; 32]);
    let key = PlayerIdentity::from_private([9; 32]).expect("curve").public();
    let ann = hub.connect_player(key, "ann", 0).expect("ann");
    assert_eq!(hub.connect_player(key, "bob", 0), Err(HubError::IdentityInUse));
    assert_eq!(
        hub.connect_player(key, " ANN ", 5),
        Ok(ann),
        "a reconnect keeps the session"
    );
    let other = PlayerIdentity::from_private([10; 32]).expect("curve").public();
    assert_eq!(hub.connect_player(other, "ann", 0), Err(HubError::HandleTaken));
    assert_eq!(hub.report().refused.get("hub_identity_in_use"), Some(&1));
}

/// Acceptance (NL-05): chat through the hub keeps its length cap and per-second rate limit as
/// settings, a mute holds (also when the muted player leaves and comes back, under a new session),
/// and the history is bounded.
#[test]
fn chat_rate_limit_and_mute_hold() {
    let settings = HubSettings {
        chat_per_sec: 3,
        chat_max_chars: 12,
        chat_history: 4,
        ..HubSettings::default()
    };
    let mut hub = HubServer::with_secret(settings, [1; 32]);
    let owner_key = PlayerIdentity::from_private([1; 32]).expect("curve").public();
    let bob_key = PlayerIdentity::from_private([2; 32]).expect("curve").public();
    let owner = hub.connect_player(owner_key, "owner", 0).expect("owner");
    let bob = hub.connect_player(bob_key, "bob", 0).expect("bob");
    let lobby = hub.create(owner, &LobbySpec::default(), 0).expect("lobby");
    hub.join(bob, lobby, None, 0).expect("join");
    let scope = ChatScope::Lobby(lobby);
    // The length cap is the setting.
    assert_eq!(
        hub.chat(bob, scope, &"x".repeat(13), 0),
        Err(HubError::ChatInvalid)
    );
    assert!(hub.chat(bob, scope, &"x".repeat(12), 0).is_ok());
    // Three lines a second, then the limit holds until the window moves.
    assert!(hub.chat(bob, scope, "two", 100).is_ok());
    assert!(hub.chat(bob, scope, "three", 200).is_ok());
    assert_eq!(hub.chat(bob, scope, "four", 300), Err(HubError::ChatRateLimited));
    assert_eq!(hub.chat(bob, scope, "four", 999), Err(HubError::ChatRateLimited));
    assert!(
        hub.chat(bob, scope, "four", 1000).is_ok(),
        "the first line left the window"
    );
    // The rate limit is per sender: the owner is not limited by bob.
    assert!(hub.chat(owner, scope, "gg", 1000).is_ok());
    // History is bounded by the setting, newest kept.
    let history = hub.chat_history(lobby);
    assert_eq!(history.len(), 4);
    assert_eq!(history.last().map(|(_, t)| t.as_str()), Some("gg"));
    // A mute holds: on the session, and on the player key after a leave and a new session.
    hub.mute(owner, lobby, bob, true, 2000).expect("mute");
    assert_eq!(hub.chat(bob, scope, "hi", 5000), Err(HubError::Muted));
    hub.leave(bob, lobby, 5000);
    hub.join(bob, lobby, None, 5000).expect("rejoin");
    assert_eq!(
        hub.chat(bob, scope, "hi", 6000),
        Err(HubError::Muted),
        "a rejoin keeps the mute"
    );
    hub.disconnect(bob);
    let bob2 = hub.connect_player(bob_key, "bob", 7000).expect("bob again");
    assert_ne!(bob2, bob, "a new session");
    hub.join(bob2, lobby, None, 7000).expect("join");
    assert_eq!(
        hub.chat(bob2, scope, "hi", 8000),
        Err(HubError::Muted),
        "a new session under the same key keeps the mute"
    );
    assert_eq!(
        hub.mute(owner, lobby, owner, true, 8000),
        Err(HubError::CannotMuteOwner)
    );
    hub.mute(owner, lobby, bob2, false, 9000).expect("unmute");
    assert!(hub.chat(bob2, scope, "thanks", 9000).is_ok());
    assert_eq!(hub.report().refused.get("hub_muted"), Some(&3));
    assert_eq!(hub.report().refused.get("hub_chat_rate_limited"), Some(&2));
}
