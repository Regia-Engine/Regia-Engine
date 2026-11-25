// SPDX-License-Identifier: MPL-2.0
//! Delegated sessions (NL-07, ADR-10384). Acceptance rows 1, 2, 6, 8, 9, 13, 14 and 15.
//!
//! Row 1 runs the existing [`HubReach`] on a simulated NAT: mapped, punch and relay. B's handshake
//! pins S's `ServerKey`. A's address is not in B's plan, the room or presence. Time is a step
//! counter. Nothing sleeps.

#![cfg(feature = "ecs")]

mod common;

use std::collections::HashMap;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};

use common::{Duel, Team};
use regia_net::crypto::binding::{
    forge_low_order_binding, forge_low_order_presence, prove_binding, prove_server_presence,
};
use regia_net::crypto::{PlayerIdentity, PlayerKey, ServerIdentity, ServerKey};
use regia_net::lobby::hub::{
    BINDING_MAX_HORIZON_MS, BindingClaims, DirectoryPage, HubError, HubEvent, HubReach, HubServer,
    HubSettings, JoinPlan, LobbyKind, NatClass, ReachKind, ReachOffer, ReachSocket, ReachState,
    ServerPresence, encode_binding_payload, set_bans, set_reach_mode,
};
use regia_net::lobby::{LobbyInfo, LobbyServer, LobbySpec, LobbyVisibility, MemberId, keys};
use regia_net::nat::{Candidate, CandidateKind};
use regia_net::prelude::*;
use regia_net::relay::{RelayConfig, RelayCore, RelayServer, SimNat};
use regia_net::server_host::{JoinTicket, RelayGamePort, RelayRegistrar, fresh_allocation_key, relay_split};
use regia_net::socket::LoopbackSocket;
use regia_net::{ClientConfig, CryptoMode, ServerConfig};

const STEP_MS: u64 = 10;
const RELAY_IP: [u8; 4] = [192, 0, 2, 1];
const LINK_HASH: [u8; 32] = [0x5A; 32];
const OWNER_IP: [u8; 4] = [198, 51, 100, 9];

fn relay_addr() -> SocketAddr {
    SocketAddr::from((RELAY_IP, 7800))
}

fn owner_addr() -> SocketAddr {
    SocketAddr::from((OWNER_IP, 4400))
}

fn game_identity() -> ServerIdentity {
    ServerIdentity::from_private([0x71; 32]).expect("curve")
}

fn hub_identity() -> ServerIdentity {
    ServerIdentity::from_private([0x41; 32]).expect("curve")
}

fn player(secret: u8) -> PlayerIdentity {
    PlayerIdentity::from_private([secret; 32]).expect("curve")
}

fn open_hub() -> (ServerIdentity, HubServer) {
    let identity = hub_identity();
    let mut hub = HubServer::with_secret(HubSettings::default(), [1; 32]);
    hub.set_hub_identity(identity.clone());
    (identity, hub)
}

fn binding_proof(
    owner: &PlayerIdentity,
    hub: &ServerKey,
    server: ServerKey,
    expiry_ms: u64,
    serial: u64,
) -> Vec<u8> {
    let claims = BindingClaims {
        version: 1,
        server_key: server,
        expiry_ms,
        serial,
    };
    let payload = encode_binding_payload(&claims).expect("payload");
    prove_binding(owner, hub, &payload).expect("binding")
}

fn presence_of(
    game: &ServerIdentity,
    hub: &ServerKey,
    hash: &[u8; 32],
    proofs: Vec<Vec<u8>>,
    grace_ms: u32,
) -> ServerPresence {
    ServerPresence {
        server_proof: prove_server_presence(game, hub, hash).expect("presence"),
        proofs,
        capacity: 8,
        load: 0,
        protocol: 1,
        game_version: "2".into(),
        relay_only: false,
        grace_ms,
        bans: Vec::new(),
    }
}

/// A keyless server session with a noted link hash, on wire 2.
fn server_session(hub: &mut HubServer, hash: [u8; 32], now: u64) -> MemberId {
    server_session_named(hub, "srv", hash, now)
}

/// A second link needs its own handle. Handles are unique on the hub.
fn server_session_named(hub: &mut HubServer, handle: &str, hash: [u8; 32], now: u64) -> MemberId {
    let member = hub.connect(handle, now).expect("server session");
    hub.note_link_hash(member, hash);
    hub.set_wire(member, 2);
    member
}

fn register(
    hub: &mut HubServer,
    member: MemberId,
    game: &ServerIdentity,
    hub_key: &ServerKey,
    hash: [u8; 32],
    proofs: Vec<Vec<u8>>,
    now: u64,
) -> Result<(), HubError> {
    let body = presence_of(game, hub_key, &hash, proofs, 0);
    hub.register_server(member, &body, now)
}

fn assert_hides_owner(plan: &JoinPlan, info: &LobbyInfo) {
    assert_ne!(plan.mapped, Some(owner_addr()));
    assert!(plan.candidates.iter().all(|c| c.addr != owner_addr()));
    assert!(!plan.ticket.windows(4).any(|w| w == OWNER_IP));
    assert!(info.server.is_none());
    assert!(info.data.values().all(|v| !v.contains("198.51.100.9")));
    assert!(
        info.members
            .iter()
            .all(|m| m.data.values().all(|v| !v.contains("198.51.100.9")))
    );
}

fn no_owner_changed(events: &[HubEvent]) -> bool {
    !events.iter().any(|e| matches!(e, HubEvent::OwnerChanged { .. }))
}

/// A game socket behind a [`SimNat`], or open when `nat` is `None`. Copied from the join harness:
/// integration tests cannot share a type across crates.
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

/// Row 1. S offers and approves. B's plan pins S. A's address never enters the plan, the room or
/// presence. The path runs until the game handshake seals.
fn reach_case(host_nat: Option<NatClass>, mapped: bool, member_nat: Option<NatClass>, expect: ReachKind) {
    let net = LoopbackNet::new();
    let control = net.bind(relay_addr()).expect("bind relay");
    let pool = (0..16u16)
        .map(|i| {
            net.bind(SocketAddr::from((RELAY_IP, 50_000 + i)))
                .expect("bind allocation port")
        })
        .collect();
    let mut relay = RelayServer::from_core(
        control,
        pool,
        RelayCore::with_config(31, [9; 32], RelayConfig::default()),
    );
    let game_id = game_identity();
    let game_socket = NatSocket::new(&net, host_nat, Ipv4Addr::new(198, 51, 100, 7), 7777);
    let (port, control) = relay_split(game_socket, relay_addr());
    let mut game: NetServer<Duel, RelayGamePort<NatSocket>> = NetServer::with_secret(
        port,
        ServerConfig {
            identity: Some(game_id.clone()),
            crypto: CryptoMode::Required,
            max_peers: 8,
            ..ServerConfig::default()
        },
        [6; 32],
    );
    let game_key = game.server_key().expect("game key");
    assert_eq!(game_key, game_id.public());
    game.add_relay_ip(relay_addr().ip());
    let ticket = JoinTicket::from_game_socket(Some(game_key));
    let mut registrar = RelayRegistrar::new(Box::new(control), relay_addr(), &ticket).expect("ticket fits");

    let mut now = 0u64;
    for _ in 0..500 {
        now += STEP_MS;
        relay.update(now);
        registrar.update(now);
        game.update(now);
        while game.poll_event().is_some() {}
        if registrar.join_code().is_some() {
            break;
        }
    }
    let code = registrar.join_code().expect("join code").to_owned();

    let (hub_id, mut hub) = open_hub();
    let hub_key = hub_id.public();
    let ann = player(0xA1);
    let server_member = server_session(&mut hub, LINK_HASH, now);
    let proof = binding_proof(&ann, &hub_key, game_key, 0, 1);
    register(
        &mut hub,
        server_member,
        &game_id,
        &hub_key,
        LINK_HASH,
        vec![proof],
        now,
    )
    .expect("register");
    let owner = hub.connect_player(ann.public(), "ann", now).expect("owner");
    hub.set_wire(owner, 2);
    let lobby = hub
        .create_delegated(owner, &LobbySpec::default(), game_key, now)
        .expect("delegated lobby");

    let mut owner_offer = ReachOffer::default();
    owner_offer.candidates = vec![Candidate::new(CandidateKind::Host, owner_addr())];
    owner_offer.nat = Some(NatClass::FULL_CONE);
    assert_eq!(
        hub.offer_reach(owner, lobby, owner_offer, now)
            .expect_err("owner offer")
            .code(),
        "hub_not_session"
    );
    assert_eq!(
        hub.confirm_session(owner, lobby, Some(LobbyServer::Address(owner_addr())), now)
            .expect_err("owner confirm")
            .code(),
        "hub_address_in_data"
    );

    let mut candidates = registrar.candidates().to_vec();
    if mapped {
        candidates.push(Candidate::new(
            CandidateKind::Mapped,
            SocketAddr::from((Ipv4Addr::new(198, 51, 100, 7), 7777)),
        ));
    }
    hub.offer_reach(
        server_member,
        lobby,
        {
            let mut offer = ReachOffer::default();
            offer.candidates = candidates;
            offer.nat = host_nat;
            offer.relay = Some(relay_addr());
            offer.join_code = Some(code);
            offer.ticket = ticket.encode();
            offer.relay_only = false;
            offer
        },
        now,
    )
    .expect("server offer");

    let bob_id = player(0xB2);
    let bob = hub.connect_player(bob_id.public(), "bob", now).expect("bob");
    hub.set_wire(bob, 2);
    hub.join(bob, lobby, None, now).expect("bob joins");
    hub.ask_reach(
        bob,
        lobby,
        {
            let mut offer = ReachOffer::default();
            offer.nat = member_nat;
            offer
        },
        now,
    )
    .expect("ask");
    let asks: Vec<MemberId> = hub
        .take_events(server_member)
        .into_iter()
        .filter_map(|e| match e {
            HubEvent::ReachAsk { member, .. } => Some(member),
            _ => None,
        })
        .collect();
    assert_eq!(asks, [bob]);
    let owner_events = hub.take_events(owner);
    assert!(no_owner_changed(&owner_events));
    assert!(
        !owner_events
            .iter()
            .any(|e| matches!(e, HubEvent::ReachAsk { .. })),
        "the owning player is not asked"
    );
    assert_eq!(
        hub.approve(owner, lobby, bob, true, now)
            .expect_err("owner approve")
            .code(),
        "hub_not_session"
    );
    let plan = hub
        .approve(server_member, lobby, bob, true, now)
        .expect("server approve")
        .expect("a plan");
    assert_eq!(plan.path, expect);
    assert_eq!(plan.server_key(), Some(game_key));
    assert_eq!(plan.peer, server_member);
    assert_eq!(
        plan.peer_identity,
        PlayerKey::default(),
        "the server session has no player key"
    );
    assert_ne!(plan.peer_identity, ann.public());
    let info = hub.lobby_info(lobby).expect("room");
    assert_hides_owner(&plan, &info);
    let presence = format!("{:?}", hub.players());
    assert!(!presence.contains("198.51.100.9"), "{presence}");
    let bob_events = format!("{:?}", hub.take_events(bob));
    assert!(!bob_events.contains("198.51.100.9"), "{bob_events}");

    let member_socket = NatSocket::new(&net, member_nat, Ipv4Addr::new(203, 0, 113, 10), 5000);
    let mut reach = HubReach::new(member_socket, &plan, fresh_allocation_key()).expect("reach");
    if expect == ReachKind::Mapped {
        assert_eq!(reach.state(), &ReachState::Ready);
    }
    let mut client: Option<GameClient> = None;
    let mut sealed = false;
    for _ in 0..3000 {
        if client.as_ref().is_some_and(NetClient::is_connected) {
            sealed = true;
            break;
        }
        now += STEP_MS;
        relay.update(now);
        registrar.update(now);
        game.update(now);
        while game.poll_event().is_some() {}
        if let ReachState::Failed(code) = reach.update(now) {
            panic!("reach failed: {code}");
        }
        if client.is_none() && reach.state() == &ReachState::Ready {
            let (socket, addr, key) = reach.take_socket().expect("socket");
            assert_eq!(key, Some(game_key), "B pins S");
            client = Some(
                NetClient::connect(
                    socket,
                    addr,
                    ClientConfig {
                        name: "bob".into(),
                        crypto: CryptoMode::Required,
                        server_key: key,
                        ..ClientConfig::default()
                    },
                    &Team(0),
                )
                .expect("join fits"),
            );
        }
        if let Some(c) = &mut client {
            c.update(now);
        }
        game.update(now);
        while game.poll_event().is_some() {}
        relay.update(now);
    }
    assert!(sealed, "handshake did not seal; reach {:?}", reach.state());
    assert!(!matches!(reach.state(), ReachState::Dialing));
    assert_eq!(game.peer_rows().len(), 1);
}

#[test]
fn mapped_delegated_reach_pins_the_server_and_hides_the_owner() {
    reach_case(None, true, Some(NatClass::SYMMETRIC), ReachKind::Mapped);
}

#[test]
fn punched_delegated_reach_pins_the_server_and_hides_the_owner() {
    reach_case(
        Some(NatClass::PORT_RESTRICTED),
        false,
        Some(NatClass::FULL_CONE),
        ReachKind::Punch,
    );
}

#[test]
fn relayed_delegated_reach_pins_the_server_and_hides_the_owner() {
    reach_case(
        Some(NatClass::SYMMETRIC),
        false,
        Some(NatClass::SYMMETRIC),
        ReachKind::Relay,
    );
}

/// Row 2. Unbound, expired and foreign owners cannot create. A revocation stops the next create
/// and leaves the lobby that already exists.
#[test]
fn unbound_expired_and_foreign_owners_are_refused_and_revocation_is_immediate() {
    let game = game_identity();
    let game_key = game.public();
    let (hub_id, mut hub) = open_hub();
    let hub_key = hub_id.public();
    let ann = player(0xA1);
    let cy = player(0xC3);

    let bare = server_session(&mut hub, LINK_HASH, 0);
    register(&mut hub, bare, &game, &hub_key, LINK_HASH, Vec::new(), 0).expect("ownerless");
    let ann_id = hub.connect_player(ann.public(), "ann", 0).expect("ann");
    hub.set_wire(ann_id, 2);
    assert_eq!(
        hub.create_delegated(ann_id, &LobbySpec::default(), game_key, 0)
            .expect_err("unbound")
            .code(),
        "hub_binding_unbound"
    );

    let expired = binding_proof(&ann, &hub_key, game_key, 50, 4);
    assert_eq!(
        register(&mut hub, bare, &game, &hub_key, LINK_HASH, vec![expired], 50)
            .expect_err("past expiry")
            .code(),
        "hub_binding_expired"
    );
    assert!(
        hub.report().servers[0].owner_count == 0,
        "a failed register changes nothing"
    );

    let live = binding_proof(&ann, &hub_key, game_key, 0, 7);
    register(&mut hub, bare, &game, &hub_key, LINK_HASH, vec![live], 100).expect("bind");
    let lobby = hub
        .create_delegated(ann_id, &LobbySpec::default(), game_key, 100)
        .expect("ann creates");
    let cy_id = hub.connect_player(cy.public(), "cy", 100).expect("cy");
    hub.set_wire(cy_id, 2);
    assert_eq!(
        hub.create_delegated(cy_id, &LobbySpec::default(), game_key, 100)
            .expect_err("not an owner")
            .code(),
        "hub_binding_not_owner"
    );

    hub.revoke_binding(ann_id, game_key, 7, 100).expect("revoke");
    assert!(hub.lobby_info(lobby).is_some(), "the existing lobby stays");
    hub.leave(ann_id, lobby, 100);
    // Leave of the owner starts grace; the lobby stays, so a second create is still refused.
    assert_eq!(
        hub.create_delegated(ann_id, &LobbySpec::default(), game_key, 200)
            .expect_err("revoked")
            .code(),
        "hub_binding_revoked"
    );
    assert!(hub.lobby_info(lobby).is_some());
}

/// A register that would add owners while an ownerless lobby exists changes nothing.
#[test]
fn binding_owners_cannot_be_added_while_an_ownerless_lobby_exists() {
    let game = game_identity();
    let (hub_id, mut hub) = open_hub();
    let hub_key = hub_id.public();
    let ann = player(0xA1);
    let server = server_session(&mut hub, LINK_HASH, 0);
    register(&mut hub, server, &game, &hub_key, LINK_HASH, Vec::new(), 0).expect("ownerless");
    let lobby = hub
        .create(server, &LobbySpec::default(), 0)
        .expect("public lobby");
    assert_eq!(hub.lobbies()[0].kind, LobbyKind::Dedicated);
    let proof = binding_proof(&ann, &hub_key, game.public(), 0, 1);
    assert_eq!(
        register(&mut hub, server, &game, &hub_key, LINK_HASH, vec![proof], 1)
            .expect_err("owners onto ownerless")
            .code(),
        "hub_binding_invalid"
    );
    assert_eq!(hub.report().servers[0].owner_count, 0);
    assert!(hub.lobby_info(lobby).is_some());
    let ann_id = hub.connect_player(ann.public(), "ann", 1).expect("ann");
    hub.set_wire(ann_id, 2);
    assert_eq!(
        hub.create_delegated(ann_id, &LobbySpec::default(), game.public(), 1)
            .expect_err("still unbound")
            .code(),
        "hub_binding_unbound"
    );
}

/// Too many proofs, a far expiry, a full server and a drain are refused before mutation.
#[test]
fn register_rejects_a_bad_presence_before_it_changes_state() {
    let game = game_identity();
    let (hub_id, mut hub) = open_hub();
    let hub_key = hub_id.public();
    let server = server_session(&mut hub, LINK_HASH, 0);
    let mut too_many = presence_of(&game, &hub_key, &LINK_HASH, vec![Vec::new(); 9], 0);
    assert_eq!(
        hub.register_server(server, &too_many, 0)
            .expect_err("nine proofs")
            .code(),
        "hub_binding_invalid"
    );
    assert!(hub.report().servers.is_empty());

    let ann = player(0xA1);
    let far = binding_proof(
        &ann,
        &hub_key,
        game.public(),
        BINDING_MAX_HORIZON_MS.saturating_add(1),
        1,
    );
    assert_eq!(
        register(&mut hub, server, &game, &hub_key, LINK_HASH, vec![far], 0)
            .expect_err("horizon")
            .code(),
        "hub_binding_invalid"
    );
    assert!(hub.report().servers.is_empty());

    too_many.proofs.clear();
    too_many.load = 5;
    too_many.capacity = 4;
    assert_eq!(
        hub.register_server(server, &too_many, 0)
            .expect_err("load")
            .code(),
        "hub_server_invalid"
    );

    let proof = binding_proof(&ann, &hub_key, game.public(), 0, 2);
    register(&mut hub, server, &game, &hub_key, LINK_HASH, vec![proof], 0).expect("good");
    hub.set_draining(true);
    let other = server_session_named(&mut hub, "srv2", [0x11; 32], 1);
    let again = binding_proof(&ann, &hub_key, game.public(), 0, 2);
    // The presence proof must match the new link's hash.
    let body = presence_of(&game, &hub_key, &[0x11; 32], vec![again], 0);
    assert_eq!(
        hub.register_server(other, &body, 1).expect_err("drain").code(),
        "hub_draining"
    );
    let ann_id = hub.connect_player(ann.public(), "ann", 1).expect("ann");
    hub.set_wire(ann_id, 2);
    assert_eq!(
        hub.create_delegated(ann_id, &LobbySpec::default(), game.public(), 1)
            .expect_err("drain create")
            .code(),
        "hub_draining"
    );
    assert_eq!(
        hub.create(ann_id, &LobbySpec::default(), 1)
            .expect_err("drain listen")
            .code(),
        "hub_draining"
    );
    assert_eq!(hub.report().servers.len(), 1, "the live server stays");
}

/// A second link takes the server. The same link refreshes load without `hub_server_in_use`.
#[test]
fn a_second_link_replaces_the_server_and_the_same_link_refreshes() {
    let game = game_identity();
    let (hub_id, mut hub) = open_hub();
    let hub_key = hub_id.public();
    let ann = player(0xA1);
    let proof = binding_proof(&ann, &hub_key, game.public(), 0, 1);
    let first = server_session(&mut hub, LINK_HASH, 0);
    register(
        &mut hub,
        first,
        &game,
        &hub_key,
        LINK_HASH,
        vec![proof.clone()],
        0,
    )
    .expect("first");
    let second_hash = [0x5B; 32];
    let second = server_session_named(&mut hub, "srv2", second_hash, 1);
    register(&mut hub, second, &game, &hub_key, second_hash, vec![proof], 1).expect("takeover");
    assert_eq!(hub.take_replaced_servers(), vec![first]);
    assert_eq!(hub.report().servers.len(), 1);
    let listen = hub
        .create(first, &LobbySpec::default(), 1)
        .expect("old link is a player session");
    assert_eq!(
        hub.lobbies()
            .into_iter()
            .find(|c| c.id == listen)
            .expect("listed")
            .kind,
        LobbyKind::Listen
    );

    let mut refresh = presence_of(
        &game,
        &hub_key,
        &second_hash,
        vec![binding_proof(&ann, &hub_key, game.public(), 0, 1)],
        0,
    );
    refresh.load = 3;
    hub.register_server(second, &refresh, 2).expect("refresh");
    assert!(hub.take_replaced_servers().is_empty());
    assert_eq!(hub.report().servers[0].load, 3);
}

/// A ban or a full server refuses the ask, so no plan is stored. An ask that was already
/// open is still forced to a denial when approve runs, and that denial carries no plan.
#[test]
fn a_ban_or_a_full_server_denies_the_ask_without_a_plan() {
    let game = game_identity();
    let (hub_id, mut hub) = open_hub();
    let hub_key = hub_id.public();
    let ann = player(0xA1);
    let bob_id = player(0xB2);
    let proof = binding_proof(&ann, &hub_key, game.public(), 0, 1);
    let server = server_session(&mut hub, LINK_HASH, 0);
    let mut body = presence_of(&game, &hub_key, &LINK_HASH, vec![proof], 0);
    body.capacity = 1;
    body.load = 1;
    body.bans = vec![bob_id.public()];
    hub.register_server(server, &body, 0).expect("register");
    let owner = hub.connect_player(ann.public(), "ann", 0).expect("ann");
    hub.set_wire(owner, 2);
    let lobby = hub
        .create_delegated(owner, &LobbySpec::default(), game.public(), 0)
        .expect("lobby");
    hub.offer_reach(
        server,
        lobby,
        {
            let mut offer = ReachOffer::default();
            offer.relay = Some(relay_addr());
            offer.join_code = Some("ABCDEF".into());
            offer.candidates = vec![Candidate::new(
                CandidateKind::Host,
                SocketAddr::from((Ipv4Addr::new(198, 51, 100, 7), 7777)),
            )];
            offer
        },
        0,
    )
    .expect("offer");
    let bob = hub.connect_player(bob_id.public(), "bob", 0).expect("bob");
    hub.set_wire(bob, 2);
    hub.join(bob, lobby, None, 0).expect("join");
    let _ = hub.take_events(bob);
    assert_eq!(
        hub.ask_reach(bob, lobby, ReachOffer::default(), 0),
        Err(HubError::Banned)
    );
    assert!(hub.joins().is_empty(), "a banned ask stores no join");

    body.bans.clear();
    hub.register_server(server, &body, 1).expect("still full");
    assert_eq!(
        hub.ask_reach(bob, lobby, ReachOffer::default(), 1)
            .expect_err("full")
            .code(),
        "hub_server_full"
    );
    assert!(hub.joins().is_empty(), "a full server stores no join");

    body.load = 0;
    body.capacity = 4;
    hub.register_server(server, &body, 2).expect("room opens");
    hub.ask_reach(bob, lobby, ReachOffer::default(), 2)
        .expect("open ask");
    body.bans = vec![bob_id.public()];
    hub.register_server(server, &body, 3)
        .expect("ban lands after the ask");
    let _ = hub.take_events(bob);
    assert_eq!(
        hub.approve(server, lobby, bob, true, 3).expect("forced deny"),
        None
    );
    assert!(
        hub.take_events(bob)
            .iter()
            .any(|e| matches!(e, HubEvent::ReachDenied { .. }))
    );
    let states: Vec<String> = hub.joins().into_iter().map(|r| r.state).collect();
    assert_eq!(states, ["denied"]);

    body.bans.clear();
    hub.register_server(server, &body, 4).expect("clear the ban");
    let cy = player(0xC4);
    let cy_id = hub.connect_player(cy.public(), "cy", 4).expect("cy");
    hub.set_wire(cy_id, 2);
    hub.join(cy_id, lobby, None, 4).expect("cy joins");
    hub.ask_reach(cy_id, lobby, ReachOffer::default(), 4)
        .expect("ask");
    let plan = hub
        .approve(server, lobby, cy_id, true, 4)
        .expect("approve")
        .expect("plan");
    assert_eq!(plan.peer, server);
    assert_eq!(plan.server_key(), None, "this offer carried no ticket");
}

/// Row 6. The expire that idles the owner starts grace and does not close the lobby. A later
/// expire at the deadline does. No `OwnerChanged`.
#[test]
fn grace_starts_when_the_owner_idles_and_closes_at_the_deadline() {
    let game = game_identity();
    let (hub_id, mut hub) = open_hub();
    let hub_key = hub_id.public();
    let ann = player(0xA1);
    let proof = binding_proof(&ann, &hub_key, game.public(), 0, 1);
    let server = server_session(&mut hub, LINK_HASH, 0);
    let body = presence_of(&game, &hub_key, &LINK_HASH, vec![proof], 5_000);
    hub.register_server(server, &body, 0).expect("register");
    let owner = hub.connect_player(ann.public(), "ann", 0).expect("ann");
    hub.set_wire(owner, 2);
    let lobby = hub
        .create_delegated(owner, &LobbySpec::default(), game.public(), 0)
        .expect("lobby");
    let bob = hub.connect_player(player(0xB2).public(), "bob", 0).expect("bob");
    hub.set_wire(bob, 2);
    hub.join(bob, lobby, None, 0).expect("join");
    let cy = hub.connect_player(player(0xC4).public(), "cy", 0).expect("cy");
    hub.set_wire(cy, 2);
    hub.join(cy, lobby, None, 0).expect("join");
    hub.kick(owner, lobby, cy, 0).expect("kick");
    assert!(hub.take_events(cy).iter().any(|e| matches!(
        e,
        HubEvent::Left {
            reason: regia_net::lobby::LeaveReason::Kicked,
            ..
        }
    )));
    assert_eq!(hub.lobby_info(lobby).expect("still open").owner, owner);

    let _ = hub.take_events(bob);
    let _ = hub.take_events(server);
    hub.heartbeat(server, 30_000).expect("server heartbeat");
    hub.heartbeat(bob, 30_000).expect("bob heartbeat");
    hub.heartbeat(cy, 30_000).expect("kicked member stays a session");
    let gone = hub.expire(30_000);
    assert_eq!(gone, vec![owner]);
    assert!(
        hub.lobby_info(lobby).is_some(),
        "grace does not close on the same expire"
    );
    assert_eq!(hub.lobby_info(lobby).expect("room").owner, MemberId(0));
    let bob_events = hub.take_events(bob);
    assert!(no_owner_changed(&bob_events));
    assert!(bob_events.iter().any(|e| matches!(
        e,
        HubEvent::GraceStarted {
            deadline_ms: 35_000,
            ..
        }
    )));
    assert!(
        !bob_events
            .iter()
            .any(|e| matches!(e, HubEvent::GraceEnded { .. }))
    );
    let server_events = hub.take_events(server);
    assert!(no_owner_changed(&server_events));
    assert!(server_events.iter().any(|e| matches!(
        e,
        HubEvent::GraceStarted {
            deadline_ms: 35_000,
            ..
        }
    )));

    hub.heartbeat(server, 35_000).expect("server");
    hub.heartbeat(bob, 35_000).expect("bob");
    hub.heartbeat(cy, 35_000).expect("cy");
    assert!(hub.expire(35_000).is_empty(), "nobody else is idle");
    assert!(hub.lobby_info(lobby).is_none(), "the deadline closes it");
    let closed = hub.take_events(bob);
    assert!(closed.iter().any(|e| matches!(e, HubEvent::Left { .. })));
    assert!(closed.iter().any(|e| matches!(e, HubEvent::GraceEnded { .. })));
    assert!(
        hub.take_events(server)
            .iter()
            .any(|e| matches!(e, HubEvent::GraceEnded { .. }))
    );
}

/// Row 15. `grace_ms` 0 has no deadline. Close needs the owner back. The server going offline
/// closes the lobby. Neither path emits `OwnerChanged`.
#[test]
fn grace_zero_holds_until_close_or_the_server_goes_offline() {
    let game = game_identity();
    let (hub_id, mut hub) = open_hub();
    let hub_key = hub_id.public();
    let ann = player(0xA1);
    let proof = binding_proof(&ann, &hub_key, game.public(), 0, 1);
    let server = server_session(&mut hub, LINK_HASH, 0);
    register(&mut hub, server, &game, &hub_key, LINK_HASH, vec![proof], 0).expect("register");
    let owner = hub.connect_player(ann.public(), "ann", 0).expect("ann");
    hub.set_wire(owner, 2);
    let lobby = hub
        .create_delegated(owner, &LobbySpec::default(), game.public(), 0)
        .expect("lobby");
    let bob = hub.connect_player(player(0xB2).public(), "bob", 0).expect("bob");
    hub.set_wire(bob, 2);
    hub.join(bob, lobby, None, 0).expect("join");
    let _ = hub.take_events(bob);
    let _ = hub.take_events(server);

    hub.heartbeat(server, 30_000).expect("server");
    hub.heartbeat(bob, 30_000).expect("bob");
    assert_eq!(hub.expire(30_000), vec![owner]);
    assert_eq!(hub.lobby_info(lobby).expect("held").owner, MemberId(0));
    let bob_events = hub.take_events(bob);
    assert!(no_owner_changed(&bob_events));
    assert!(
        bob_events
            .iter()
            .any(|e| matches!(e, HubEvent::GraceStarted { deadline_ms: 0, .. }))
    );
    let server_events = hub.take_events(server);
    assert!(no_owner_changed(&server_events));
    assert!(
        server_events
            .iter()
            .any(|e| matches!(e, HubEvent::GraceStarted { deadline_ms: 0, .. }))
    );

    hub.heartbeat(server, 10_000_000).expect("server");
    hub.heartbeat(bob, 10_000_000).expect("bob");
    assert!(hub.expire(10_000_000).is_empty());
    assert!(hub.lobby_info(lobby).is_some(), "no deadline");
    assert!(
        !hub.take_events(bob)
            .iter()
            .any(|e| matches!(e, HubEvent::GraceEnded { .. }))
    );

    assert_eq!(
        hub.close_lobby(server, lobby, 10_000_000)
            .expect_err("server cannot close")
            .code(),
        "hub_binding_not_owner"
    );
    let _ = hub.take_events(bob);
    let again = hub
        .connect_player(ann.public(), "ann", 10_000_000)
        .expect("reconnect");
    assert_ne!(again, owner);
    assert_eq!(hub.lobby_info(lobby).expect("reattached").owner, again);
    assert!(no_owner_changed(&hub.take_events(bob)));
    hub.close_lobby(again, lobby, 10_000_000).expect("owner closes");
    assert!(hub.lobby_info(lobby).is_none());

    let proof2 = binding_proof(&ann, &hub_key, game.public(), 0, 1);
    register(
        &mut hub,
        server,
        &game,
        &hub_key,
        LINK_HASH,
        vec![proof2],
        10_000_000,
    )
    .expect("still online");
    let owner2 = hub
        .connect_player(ann.public(), "ann", 10_000_000)
        .expect("ann again");
    assert_eq!(owner2, again, "the live session is reused");
    let lobby2 = hub
        .create_delegated(owner2, &LobbySpec::default(), game.public(), 10_000_000)
        .expect("second lobby");
    hub.heartbeat(server, 10_030_000).expect("server");
    let _ = hub.expire(10_030_000);
    assert!(hub.lobby_info(lobby2).is_some());
    hub.disconnect(server);
    assert!(hub.lobby_info(lobby2).is_none(), "server offline closes grace");
}

/// Row 8. An ownerless server lists a public dedicated lobby. The card names the server and
/// carries no address. A wire-1 join is refused and the session stays.
#[test]
fn an_ownerless_server_lists_a_public_lobby_with_no_address() {
    let game = game_identity();
    let (hub_id, mut hub) = open_hub();
    let hub_key = hub_id.public();
    let server = server_session(&mut hub, LINK_HASH, 0);
    register(&mut hub, server, &game, &hub_key, LINK_HASH, Vec::new(), 0).expect("ownerless");
    let hidden = LobbySpec {
        visibility: LobbyVisibility::Private,
        ..LobbySpec::default()
    };
    assert_eq!(
        hub.create(server, &hidden, 0).expect_err("not public").code(),
        "hub_server_invalid"
    );
    let mut addressed = LobbySpec::default();
    addressed
        .data
        .insert(keys::SERVER.into(), LobbyServer::Address(owner_addr()).to_key());
    assert_eq!(
        hub.create(server, &addressed, 0).expect_err("address").code(),
        "hub_address_in_data"
    );
    let lobby = hub.create(server, &LobbySpec::default(), 0).expect("public");
    let card = hub.lobbies().into_iter().find(|c| c.id == lobby).expect("listed");
    assert_eq!(card.kind, LobbyKind::Dedicated);
    assert_eq!(card.owner_handle, "srv");
    assert!(!card.data.contains_key(keys::SERVER));
    assert!(!format!("{card:?}").contains("198.51.100.9"));
    let info = hub.lobby_info(lobby).expect("room");
    assert!(info.server.is_none());
    assert_eq!(hub.report().servers.len(), 1);
    assert!(!format!("{:?}", hub.report().servers).contains("198.51.100.9"));

    let joiner = hub.connect_player(player(0xB2).public(), "bob", 0).expect("bob");
    assert_eq!(hub.wire(joiner), 1);
    assert_eq!(
        hub.join(joiner, lobby, None, 0).expect_err("wire 1").code(),
        "hub_needs_wire_v2"
    );
    assert!(hub.identity(joiner).is_some(), "the session stays");
    hub.set_wire(joiner, 2);
    hub.join(joiner, lobby, None, 0).expect("wire 2 joins");
    assert!(
        hub.lobby_info(lobby)
            .expect("members")
            .members
            .iter()
            .any(|m| m.id == joiner)
    );

    assert_eq!(
        hub.create_delegated(server, &LobbySpec::default(), game.public(), 0)
            .expect_err("a server is not a player")
            .code(),
        "hub_not_a_player"
    );
}

/// Wire 1 still sees listen lobbies and not dedicated ones. A listen confirm stores no address.
#[test]
fn wire_1_directory_hides_dedicated_lobbies_and_listen_still_confirms() {
    let game = game_identity();
    let (hub_id, mut hub) = open_hub();
    let hub_key = hub_id.public();
    let host = hub
        .connect_player(player(0x11).public(), "host", 0)
        .expect("host");
    let listen = hub.create(host, &LobbySpec::default(), 0).expect("listen");
    let member = hub.connect_player(player(0x12).public(), "mem", 0).expect("mem");
    hub.join(member, listen, None, 0).expect("join listen");
    let _ = hub.take_events(member);
    hub.confirm_session(host, listen, Some(LobbyServer::Address(owner_addr())), 0)
        .expect("listen still confirms");
    assert_eq!(hub.lobby_info(listen).expect("listen").server, None);
    assert!(hub.take_events(member).iter().any(|e| matches!(
        e,
        HubEvent::SessionConfirmed { lobby, confirmed: true } if *lobby == listen
    )));
    // A listen create that carries `regia.server` is refused and does not create.
    let host2 = hub
        .connect_player(player(0x14).public(), "host2", 0)
        .expect("host2");
    let mut spec = LobbySpec::default();
    spec.data
        .insert(keys::SERVER.into(), LobbyServer::Address(owner_addr()).to_key());
    assert_eq!(
        hub.create(host2, &spec, 0).expect_err("address in spec").code(),
        "hub_address_in_data"
    );
    assert!(
        hub.players()
            .iter()
            .any(|p| p.member == host2 && p.lobby.is_none()),
        "a refused create must not enter a lobby"
    );

    let server = server_session(&mut hub, LINK_HASH, 0);
    register(&mut hub, server, &game, &hub_key, LINK_HASH, Vec::new(), 0).expect("server");
    let dedicated = hub.create(server, &LobbySpec::default(), 0).expect("dedicated");

    let watcher = hub.connect("watch", 0).expect("watch");
    assert_eq!(hub.wire(watcher), 1);
    hub.flush_member(watcher);
    let pages = directory_of(&mut hub, watcher);
    assert!(pages.iter().all(|p| p.servers.is_empty()));
    assert!(
        pages
            .iter()
            .all(|p| p.lobbies.iter().all(|c| c.kind == LobbyKind::Listen))
    );
    assert!(pages.iter().any(|p| p.lobbies.iter().any(|c| c.id == listen)));
    assert!(pages.iter().all(|p| p.lobbies.iter().all(|c| c.id != dedicated)));
    assert!(pages.iter().all(|p| !format!("{p:?}").contains("198.51.100.9")));

    hub.set_wire(watcher, 2);
    hub.flush_member(watcher);
    let pages = directory_of(&mut hub, watcher);
    assert!(pages.iter().any(|p| {
        p.lobbies
            .iter()
            .any(|c| c.id == dedicated && c.kind == LobbyKind::Dedicated)
    }));
    assert!(
        pages
            .iter()
            .any(|p| p.servers.iter().any(|s| s.server_key == game.public()))
    );
    assert!(pages.iter().all(|p| !format!("{p:?}").contains("198.51.100.9")));

    let late = hub
        .connect_player(player(0x13).public(), "late", 0)
        .expect("late");
    assert_eq!(
        hub.join(late, dedicated, None, 0).expect_err("needs v2").code(),
        "hub_needs_wire_v2"
    );
    set_reach_mode(&mut hub, server, dedicated, true, 0).expect("the bound server stores the lobby flag");
    assert!(
        hub.take_events(server).iter().any(|event| matches!(
            event,
            HubEvent::Room(room) if room.lobby == dedicated && room.relay_only
        )),
        "SetReachMode stores relay_only on the room the members see"
    );
    assert_eq!(
        set_bans(&mut hub, server, dedicated, &[], 0)
            .expect_err("later lane")
            .code(),
        "hub_not_ready"
    );
}

fn directory_of(hub: &mut HubServer, member: MemberId) -> Vec<DirectoryPage> {
    hub.take_events(member)
        .into_iter()
        .filter_map(|e| match e {
            HubEvent::Directory(page) => Some(page),
            _ => None,
        })
        .collect()
}

/// Row 9. `revocations.ron` reloads. A missing file is empty. A corrupt file is an error and
/// leaves the in-memory set.
#[test]
fn revocations_reload_and_a_bad_file_is_an_error() {
    let dir = regia_test_utils::TempDir::new("v071-delegated-revoke");
    let path = dir.to_path_buf();
    let game = game_identity();
    let game_key = game.public();
    let (hub_id, mut hub) = open_hub();
    let hub_key = hub_id.public();
    assert!(
        hub.set_revocation_dir(path.clone()).is_ok(),
        "a missing file is empty"
    );
    let ann = player(0xA1);
    let proof = binding_proof(&ann, &hub_key, game_key, 0, 11);
    let server = server_session(&mut hub, LINK_HASH, 0);
    register(&mut hub, server, &game, &hub_key, LINK_HASH, vec![proof], 0).expect("register");
    let owner = hub.connect_player(ann.public(), "ann", 0).expect("ann");
    hub.set_wire(owner, 2);
    let lobby = hub
        .create_delegated(owner, &LobbySpec::default(), game_key, 0)
        .expect("lobby");
    hub.revoke_binding(owner, game_key, 11, 0).expect("revoke");
    assert!(hub.lobby_info(lobby).is_some());
    let file = path.join("revocations.ron");
    let text = std::fs::read_to_string(&file).expect("revocations.ron");
    assert!(text.contains("format:"));
    assert!(text.contains("11"));

    let (hub_id2, mut restarted) = open_hub();
    restarted.set_revocation_dir(path.clone()).expect("reload");
    let server2 = server_session(&mut restarted, LINK_HASH, 0);
    let proof2 = binding_proof(&ann, &hub_id2.public(), game_key, 0, 11);
    register(
        &mut restarted,
        server2,
        &game,
        &hub_id2.public(),
        LINK_HASH,
        vec![proof2],
        0,
    )
    .expect("register again");
    let owner2 = restarted.connect_player(ann.public(), "ann", 0).expect("ann");
    restarted.set_wire(owner2, 2);
    assert_eq!(
        restarted
            .create_delegated(owner2, &LobbySpec::default(), game_key, 0)
            .expect_err("reloaded revocation")
            .code(),
        "hub_binding_revoked"
    );

    std::fs::write(&file, "this is not ron").expect("corrupt");
    assert!(
        restarted.load_revocations().is_err(),
        "a corrupt file is an error"
    );
    assert_eq!(
        restarted
            .create_delegated(owner2, &LobbySpec::default(), game_key, 1)
            .expect_err("memory kept")
            .code(),
        "hub_binding_revoked"
    );

    let empty = regia_test_utils::TempDir::new("v071-delegated-revoke-empty");
    let (hub_id3, mut fresh) = open_hub();
    fresh
        .set_revocation_dir(empty.to_path_buf())
        .expect("missing file");
    let server3 = server_session(&mut fresh, LINK_HASH, 0);
    let proof3 = binding_proof(&ann, &hub_id3.public(), game_key, 0, 11);
    register(
        &mut fresh,
        server3,
        &game,
        &hub_id3.public(),
        LINK_HASH,
        vec![proof3],
        0,
    )
    .expect("register");
    let owner3 = fresh.connect_player(ann.public(), "ann", 0).expect("ann");
    fresh.set_wire(owner3, 2);
    fresh
        .create_delegated(owner3, &LobbySpec::default(), game_key, 0)
        .expect("an empty set does not revoke");
}

/// Row 13. A low-order static key is `hub_binding_invalid` and registers nothing.
#[test]
fn a_low_order_binding_is_invalid() {
    let game = game_identity();
    let (hub_id, mut hub) = open_hub();
    let hub_key = hub_id.public();
    let claims = BindingClaims {
        version: 1,
        server_key: game.public(),
        expiry_ms: 0,
        serial: 1,
    };
    let payload = encode_binding_payload(&claims).expect("payload");
    let forged = forge_low_order_binding(&hub_key, &payload, [0; 32], true).expect("forge");
    let server = server_session(&mut hub, LINK_HASH, 0);
    let body = ServerPresence {
        server_proof: prove_server_presence(&game, &hub_key, &LINK_HASH).expect("presence"),
        proofs: vec![forged],
        capacity: 4,
        load: 0,
        protocol: 1,
        game_version: "2".into(),
        relay_only: false,
        grace_ms: 0,
        bans: Vec::new(),
    };
    assert_eq!(
        hub.register_server(server, &body, 0)
            .expect_err("low order")
            .code(),
        "hub_binding_invalid"
    );
    assert!(hub.report().servers.is_empty());

    let weak = forge_low_order_presence(&hub_key, &LINK_HASH, [0; 32], true).expect("forge presence");
    let body = ServerPresence {
        server_proof: weak,
        ..body
    };
    assert_eq!(
        hub.register_server(server, &body, 0)
            .expect_err("low order presence")
            .code(),
        "hub_binding_invalid"
    );
    assert!(hub.report().servers.is_empty());
}

/// Row 14. Two owner proofs. A stranger is `hub_binding_not_owner`. Transfer to the server key
/// is refused. Transfer to the other live owner may emit `OwnerChanged`.
#[test]
fn two_owners_can_create_and_a_stranger_cannot() {
    let game = game_identity();
    let game_key = game.public();
    let (hub_id, mut hub) = open_hub();
    let hub_key = hub_id.public();
    let ann = player(0xA1);
    let bee = player(0xA2);
    let cy = player(0xC3);
    let proofs = vec![
        binding_proof(&ann, &hub_key, game_key, 0, 1),
        binding_proof(&bee, &hub_key, game_key, 0, 2),
    ];
    let server = server_session(&mut hub, LINK_HASH, 0);
    register(&mut hub, server, &game, &hub_key, LINK_HASH, proofs, 0).expect("two owners");
    assert_eq!(hub.report().servers[0].owner_count, 2);
    let a = hub.connect_player(ann.public(), "ann", 0).expect("ann");
    let b = hub.connect_player(bee.public(), "bee", 0).expect("bee");
    let c = hub.connect_player(cy.public(), "cy", 0).expect("cy");
    hub.set_wire(a, 2);
    hub.set_wire(b, 2);
    hub.set_wire(c, 2);
    let lobby = hub
        .create_delegated(a, &LobbySpec::default(), game_key, 0)
        .expect("ann");
    hub.create_delegated(b, &LobbySpec::default(), game_key, 0)
        .expect("bee");
    assert_eq!(
        hub.create_delegated(c, &LobbySpec::default(), game_key, 0)
            .expect_err("stranger")
            .code(),
        "hub_binding_not_owner"
    );
    assert_eq!(
        hub.transfer_delegated(a, lobby, PlayerKey(game_key.0), 0)
            .expect_err("not to the server")
            .code(),
        "hub_binding_not_owner"
    );
    let _ = hub.take_events(a);
    hub.transfer_delegated(a, lobby, bee.public(), 0)
        .expect("to the other owner");
    assert_eq!(hub.lobby_info(lobby).expect("moved").owner, b);
    assert!(
        hub.take_events(a)
            .iter()
            .any(|e| matches!(e, HubEvent::OwnerChanged { owner, .. } if *owner == b))
    );
}

/// Listen hand-off is unchanged: the next member becomes owner and hears `OwnerChanged`.
/// Close of a listen lobby stays `hub_not_ready`.
#[test]
fn a_listen_lobby_still_hands_off_with_owner_changed() {
    let (_, mut hub) = open_hub();
    let host = hub
        .connect_player(player(0x21).public(), "host", 0)
        .expect("host");
    let bob = hub.connect_player(player(0x22).public(), "bob", 0).expect("bob");
    let lobby = hub.create(host, &LobbySpec::default(), 0).expect("listen");
    hub.join(bob, lobby, None, 0).expect("join");
    assert_eq!(
        hub.close_lobby(host, lobby, 0).expect_err("listen close").code(),
        "hub_not_ready"
    );
    assert!(hub.lobby_info(lobby).is_some());
    let _ = hub.take_events(bob);
    hub.leave(host, lobby, 0);
    assert_eq!(hub.lobby_info(lobby).expect("handed off").owner, bob);
    assert!(hub.take_events(bob).iter().any(|e| matches!(
        e,
        HubEvent::OwnerChanged { owner, .. } if *owner == bob
    )));
}

/// A non-owner leaving a delegated lobby is an ordinary leave. The room stays, with no grace.
#[test]
fn a_member_leaving_a_delegated_lobby_does_not_start_grace() {
    let game = game_identity();
    let (hub_id, mut hub) = open_hub();
    let hub_key = hub_id.public();
    let ann = player(0xA1);
    let proof = binding_proof(&ann, &hub_key, game.public(), 0, 1);
    let server = server_session(&mut hub, LINK_HASH, 0);
    register(&mut hub, server, &game, &hub_key, LINK_HASH, vec![proof], 0).expect("register");
    let owner = hub.connect_player(ann.public(), "ann", 0).expect("ann");
    hub.set_wire(owner, 2);
    let lobby = hub
        .create_delegated(owner, &LobbySpec::default(), game.public(), 0)
        .expect("lobby");
    let bob = hub.connect_player(player(0xB2).public(), "bob", 0).expect("bob");
    hub.set_wire(bob, 2);
    hub.join(bob, lobby, None, 0).expect("join");
    let _ = hub.take_events(owner);
    let _ = hub.take_events(server);
    hub.leave(bob, lobby, 0);
    assert_eq!(hub.lobby_info(lobby).expect("stays").owner, owner);
    assert!(no_owner_changed(&hub.take_events(owner)));
    assert!(
        !hub.take_events(server)
            .iter()
            .any(|e| matches!(e, HubEvent::GraceStarted { .. }))
    );
}

/// Review. A resumed owner link resyncs through [`HubServer::resync`] (the host does this on
/// `PeerResumed`). Pending asks belong to the server session. The owning player must not be handed
/// the member's candidates, and the server must still see the ask.
#[test]
fn review_resync_does_not_give_the_owner_member_candidates() {
    let game = game_identity();
    let (hub_id, mut hub) = open_hub();
    let hub_key = hub_id.public();
    let proof = binding_proof(&player(0xA1), &hub_key, game.public(), 0, 1);
    let server = server_session(&mut hub, LINK_HASH, 0);
    register(&mut hub, server, &game, &hub_key, LINK_HASH, vec![proof], 0).expect("register");
    let owner = hub.connect_player(player(0xA1).public(), "ann", 0).expect("ann");
    hub.set_wire(owner, 2);
    let lobby = hub
        .create_delegated(owner, &LobbySpec::default(), game.public(), 0)
        .expect("lobby");
    let server_addr = SocketAddr::from((Ipv4Addr::new(198, 51, 100, 7), 7777));
    hub.offer_reach(
        server,
        lobby,
        {
            let mut offer = ReachOffer::default();
            offer.candidates = vec![Candidate::new(CandidateKind::Host, server_addr)];
            offer.relay = Some(relay_addr());
            offer.join_code = Some("ABCDEF".into());
            offer
        },
        0,
    )
    .expect("server offer");
    let bob = hub.connect_player(player(0xB2).public(), "bob", 0).expect("bob");
    hub.set_wire(bob, 2);
    hub.join(bob, lobby, None, 0).expect("join");
    let member_addr = SocketAddr::from((Ipv4Addr::new(203, 0, 113, 10), 5000));
    hub.ask_reach(
        bob,
        lobby,
        {
            let mut offer = ReachOffer::default();
            offer.candidates = vec![Candidate::new(CandidateKind::Host, member_addr)];
            offer
        },
        0,
    )
    .expect("ask");
    let _ = hub.take_events(owner);
    let _ = hub.take_events(server);
    let _ = hub.take_events(bob);

    hub.resync(owner);
    let owner_dump = format!("{:?}", hub.take_events(owner));
    assert!(
        !owner_dump.contains("203.0.113.10"),
        "the owning player learned the member's address on resync: {owner_dump}"
    );
    assert!(
        !owner_dump.contains("ReachAsk"),
        "the owning player was asked on resync: {owner_dump}"
    );
    assert!(
        !owner_dump.contains("198.51.100.7"),
        "the owning player learned the server's address on resync: {owner_dump}"
    );

    hub.resync(server);
    let server_dump = format!("{:?}", hub.take_events(server));
    assert!(
        server_dump.contains("203.0.113.10"),
        "the server session lost the pending ask on resync: {server_dump}"
    );
}

/// Review. One link may prove the owner and register the server. A second link takes the server
/// key back. Spec 89 § 16.3: that drop starts grace and never assigns ownership to the server
/// session. `grace_ms` 0 keeps the lobby. The same player key reconnects as owner.
#[test]
fn review_server_reconnect_does_not_take_the_delegated_owner() {
    let game = game_identity();
    let (hub_id, mut hub) = open_hub();
    let hub_key = hub_id.public();
    let ann = player(0xA1);
    let owner = hub.connect_player(ann.public(), "ann", 0).expect("ann");
    hub.note_link_hash(owner, LINK_HASH);
    hub.set_wire(owner, 2);
    let proof = binding_proof(&ann, &hub_key, game.public(), 0, 1);
    register(
        &mut hub,
        owner,
        &game,
        &hub_key,
        LINK_HASH,
        vec![proof.clone()],
        0,
    )
    .expect("register");
    let lobby = hub
        .create_delegated(owner, &LobbySpec::default(), game.public(), 0)
        .expect("delegated");
    let bob = hub.connect_player(player(0xB2).public(), "bob", 0).expect("bob");
    hub.set_wire(bob, 2);
    hub.join(bob, lobby, None, 0).expect("join");
    let _ = hub.take_events(bob);

    let second_hash = [0x5B; 32];
    let second = server_session_named(&mut hub, "srv2", second_hash, 1);
    register(&mut hub, second, &game, &hub_key, second_hash, vec![proof], 1).expect("server link replaced");
    assert_eq!(hub.take_replaced_servers(), vec![owner]);
    hub.disconnect(owner);

    let info = hub.lobby_info(lobby).expect("grace keeps the lobby");
    assert_eq!(info.owner, MemberId(0), "the new server session became the owner");
    assert_ne!(info.owner, second);
    assert!(no_owner_changed(&hub.take_events(bob)));
    assert_eq!(
        hub.set_joinable(second, lobby, false, 1)
            .expect_err("server is not the owner")
            .code(),
        "not_owner"
    );

    let again = hub.connect_player(ann.public(), "ann", 2).expect("owner returns");
    assert_eq!(hub.lobby_info(lobby).expect("reattached").owner, again);
    assert!(no_owner_changed(&hub.take_events(bob)));
}

/// Review. Registering a server on a link that already owns a listen lobby, then taking that
/// server key from a second link, drops the old link (`HubHost` disconnects it). Listen hand-off
/// still has to run. A lobby with no other member is deleted. A lobby with a guest goes to that
/// guest. Leaving the room listed under the dead member fills `lobby_cap` after the attacker is gone.
#[test]
fn review_replaced_listen_owner_is_not_left_in_the_directory() {
    let game = game_identity();
    let (hub_id, mut hub) = open_hub();
    let hub_key = hub_id.public();

    let solo_host = hub
        .connect_player(player(0x31).public(), "solo", 0)
        .expect("solo");
    let solo = hub
        .create(solo_host, &LobbySpec::default(), 0)
        .expect("public listen");
    hub.note_link_hash(solo_host, LINK_HASH);
    register(&mut hub, solo_host, &game, &hub_key, LINK_HASH, Vec::new(), 0).expect("ownerless");
    let second_hash = [0x5B; 32];
    let second = server_session_named(&mut hub, "srv2", second_hash, 1);
    register(&mut hub, second, &game, &hub_key, second_hash, Vec::new(), 1).expect("takeover");
    assert_eq!(hub.take_replaced_servers(), vec![solo_host]);
    hub.disconnect(solo_host);
    assert!(
        hub.lobby_info(solo).is_none(),
        "the only member is gone and the public lobby is still listed"
    );
    assert!(
        hub.lobbies().iter().all(|card| card.id != solo),
        "the directory still advertises the dead lobby"
    );

    let host = hub
        .connect_player(player(0x32).public(), "host", 2)
        .expect("host");
    let guest = hub
        .connect_player(player(0x33).public(), "guest", 2)
        .expect("guest");
    let lobby = hub.create(host, &LobbySpec::default(), 2).expect("listen");
    hub.join(guest, lobby, None, 2).expect("guest joins");
    let _ = hub.take_events(guest);
    let host_hash = [0x6C; 32];
    hub.note_link_hash(host, host_hash);
    let game2 = ServerIdentity::from_private([0x72; 32]).expect("curve");
    register(&mut hub, host, &game2, &hub_key, host_hash, Vec::new(), 2).expect("ownerless");
    let next_hash = [0x6D; 32];
    let next = server_session_named(&mut hub, "srv3", next_hash, 3);
    register(&mut hub, next, &game2, &hub_key, next_hash, Vec::new(), 3).expect("takeover");
    assert_eq!(hub.take_replaced_servers(), vec![host]);
    hub.disconnect(host);
    assert_eq!(
        hub.lobby_info(lobby).expect("guest remains").owner,
        guest,
        "listen hand-off did not run"
    );
    assert!(hub.take_events(guest).iter().any(|e| matches!(
        e,
        HubEvent::OwnerChanged { owner, .. } if *owner == guest
    )));
}
