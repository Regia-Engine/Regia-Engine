// SPDX-License-Identifier: MPL-2.0
//! NL-08 acceptance (spec 89 § 16.10 rows 3 and 16, ADR-10391).
//!
//! Plan bytes leave the hub through [`HubEvent::to_bytes`]. Relay `Register`, `PunchIntro` and
//! connectivity-check datagrams are captured off the sockets that send them. A relay-only pair
//! fails when the other side's address octets appear in those payloads. A run with the flag clear
//! must show the host octets in `Register` and the member octets in `PunchIntro`, so a capture
//! that recorded nothing cannot pass.
//!
//! The review tests at the bottom of this file require the stronger promise: no address of the
//! other side in any byte either party receives, including `HostInfo`, `Allocate`, `Allocated`,
//! `ClientJoined` and `ReachAsk`, for an offer flag and for a stored server or lobby flag.
//! Those tests fail while the replies and the ask still carry the address. Time is a step
//! counter. Nothing sleeps.

#![cfg(feature = "ecs")]

use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use regia_net::crypto::binding::prove_server_presence;
use regia_net::crypto::{PlayerKey, ServerIdentity, ServerKey};
use regia_net::lobby::hub::{
    HubError, HubEvent, HubReach, HubRequest, HubServer, HubSettings, JoinPlan, NatClass, ReachKind,
    ReachOffer, ServerPresence, set_reach_mode,
};
use regia_net::lobby::{LobbyId, LobbySpec, MemberId};
use regia_net::nat::{Candidate, CandidateKind, NAT_CHECK_MAGIC};
use regia_net::relay::{
    RELAY_ALLOCATION_KEY_BYTES, RELAY_MAGIC, RELAY_VERSIONED, RelayConfig, RelayCore, RelayServer, RelayTag,
};
use regia_net::server_host::{JoinPath, JoinTicket, RelayRegistrar};
use regia_net::socket::{DatagramSocket, LoopbackNet, LoopbackSocket};

const HOST: [u8; 4] = [198, 51, 100, 7];
const MAPPED: [u8; 4] = [198, 51, 100, 50];
const MEMBER: [u8; 4] = [203, 0, 113, 20];
const OPEN: [u8; 4] = [203, 0, 113, 21];
const RELAY_IP: [u8; 4] = [192, 0, 2, 1];
const STEP_MS: u64 = 250;
/// Not a relay tag. Marks a connectivity check (`RGNC`).
const CHECK: u8 = 0xFE;

fn addr(ip: [u8; 4], port: u16) -> SocketAddr {
    SocketAddr::from((ip, port))
}

fn host() -> SocketAddr {
    addr(HOST, 7777)
}

fn mapped() -> SocketAddr {
    addr(MAPPED, 7777)
}

fn member_addr() -> SocketAddr {
    addr(MEMBER, 5000)
}

fn open_addr() -> SocketAddr {
    addr(OPEN, 5001)
}

fn relay_addr() -> SocketAddr {
    addr(RELAY_IP, 7800)
}

fn server_key() -> ServerKey {
    ServerKey([0x11; 32])
}

fn has_octets(bytes: &[u8], ip: [u8; 4]) -> bool {
    bytes.windows(4).any(|window| window == ip)
}

fn datagram_kind(bytes: &[u8]) -> Option<u8> {
    if bytes.starts_with(&NAT_CHECK_MAGIC) {
        return Some(CHECK);
    }
    if bytes.len() < 5 || !bytes.starts_with(&RELAY_MAGIC) {
        return None;
    }
    if bytes[4] != RELAY_VERSIONED {
        return Some(bytes[4]);
    }
    (bytes.len() >= 7).then_some(bytes[6])
}

fn is_sensitive(kind: u8) -> bool {
    kind == CHECK || kind == RelayTag::Register as u8 || kind == RelayTag::PunchIntro as u8
}

struct Sent {
    bytes: Vec<u8>,
}

struct Tape {
    inner: LoopbackSocket,
    log: Arc<Mutex<Vec<Sent>>>,
}

fn tape(net: &LoopbackNet, at: SocketAddr, log: &Arc<Mutex<Vec<Sent>>>) -> Tape {
    Tape {
        inner: net.bind(at).expect("bind"),
        log: Arc::clone(log),
    }
}

impl DatagramSocket for Tape {
    fn send_to(&mut self, bytes: &[u8], dest: SocketAddr) -> io::Result<()> {
        self.log.lock().expect("send log").push(Sent {
            bytes: bytes.to_vec(),
        });
        self.inner.send_to(bytes, dest)
    }

    fn recv_from(&mut self, buf: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
        self.inner.recv_from(buf)
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }
}

fn log_of(log: &Arc<Mutex<Vec<Sent>>>) -> Vec<Sent> {
    std::mem::take(&mut *log.lock().expect("send log"))
}

fn sensitive(rows: &[Sent]) -> Vec<(u8, &[u8])> {
    rows.iter()
        .filter_map(|row| datagram_kind(&row.bytes).map(|kind| (kind, row.bytes.as_slice())))
        .filter(|(kind, _)| is_sensitive(*kind))
        .collect()
}

fn assert_clean(rows: &[Sent], forbidden: &[[u8; 4]]) {
    for (kind, bytes) in sensitive(rows) {
        for ip in forbidden {
            assert!(
                !has_octets(bytes, *ip),
                "tag {kind} carried {ip:?} in {} bytes",
                bytes.len()
            );
        }
    }
}

fn count_kind(rows: &[Sent], kind: u8) -> usize {
    rows.iter()
        .filter(|row| datagram_kind(&row.bytes) == Some(kind))
        .count()
}

// `ReachOffer` is `#[non_exhaustive]` from v0.7.2 on: built with `Default` plus assignment.
fn offer(relay_only: bool) -> ReachOffer {
    let mut offer = ReachOffer::default();
    offer.candidates = vec![
        Candidate::new(CandidateKind::Host, host()),
        Candidate::new(CandidateKind::Mapped, mapped()),
    ];
    offer.nat = Some(NatClass::FULL_CONE);
    offer.relay = Some(relay_addr());
    offer.join_code = Some("ABCDEF".to_owned());
    offer.ticket = JoinTicket {
        addr: host(),
        server_key: Some(server_key()),
    }
    .encode();
    offer.relay_only = relay_only;
    offer
}

fn ask(at: SocketAddr, relay_only: bool) -> ReachOffer {
    let mut offer = ReachOffer::default();
    offer.candidates = vec![Candidate::new(CandidateKind::Host, at)];
    offer.nat = Some(NatClass::PORT_RESTRICTED);
    offer.relay_only = relay_only;
    offer
}

struct Listen {
    hub: HubServer,
    owner: MemberId,
    member: MemberId,
    open: MemberId,
    lobby: LobbyId,
}

fn listen() -> Listen {
    let mut hub = HubServer::with_secret(HubSettings::default(), [3; 32]);
    let owner = hub.connect_player(PlayerKey([1; 32]), "owner", 0).expect("owner");
    let member = hub
        .connect_player(PlayerKey([2; 32]), "member", 0)
        .expect("member");
    let open = hub.connect_player(PlayerKey([4; 32]), "open", 0).expect("open");
    let lobby = hub.create(owner, &LobbySpec::default(), 0).expect("lobby");
    hub.join(member, lobby, None, 0).expect("member joins");
    hub.join(open, lobby, None, 0).expect("open joins");
    Listen {
        hub,
        owner,
        member,
        open,
        lobby,
    }
}

fn plan_bytes(events: &[HubEvent], peer: MemberId) -> Vec<u8> {
    let plan = events.iter().find_map(|event| match event {
        HubEvent::Plan(plan) if plan.peer == peer => Some(plan.clone()),
        _ => None,
    });
    let Some(plan) = plan else {
        panic!("no plan for {peer:?}");
    };
    HubEvent::Plan(plan).to_bytes().expect("plan bytes")
}

fn assert_rewritten(plan: &JoinPlan) {
    assert_eq!(plan.path, ReachKind::Relay);
    assert!(plan.candidates.is_empty());
    assert!(plan.mapped.is_none());
    assert_eq!(plan.relay, Some(relay_addr()));
    assert_eq!(plan.join_code.as_deref(), Some("ABCDEF"));
    let ticket = JoinTicket::decode(&plan.ticket).expect("rewritten ticket");
    assert!(ticket.addr.ip().is_unspecified() && ticket.addr.port() == 0);
    assert_eq!(ticket.server_key, Some(server_key()));
}

/// The owner's `relay_only` offer, approved through `HubServer` (the lobby flag stays clear).
/// Both plan payloads omit the host, the mapped address and the member.
#[test]
fn owner_relay_only_plan_bytes_omit_both_addresses() {
    let mut world = listen();
    let Listen {
        hub,
        owner,
        member,
        lobby,
        ..
    } = &mut world;
    hub.offer_reach(*owner, *lobby, offer(true), 0).expect("offer");
    hub.ask_reach(*member, *lobby, ask(member_addr(), false), 0)
        .expect("ask");
    let plan = hub
        .approve(*owner, *lobby, *member, true, 0)
        .expect("approve")
        .expect("a plan");
    assert_rewritten(&plan);
    let member_bytes = plan_bytes(&hub.take_events(*member), *owner);
    let owner_bytes = plan_bytes(&hub.take_events(*owner), *member);
    assert!(!has_octets(&member_bytes, HOST));
    assert!(!has_octets(&member_bytes, MAPPED));
    assert!(!has_octets(&owner_bytes, MEMBER));
    assert!(
        has_octets(&member_bytes, RELAY_IP),
        "the relay address may travel"
    );
}

/// One member's flag hides that pair. A member who left the flag clear still gets the mapped
/// path, and those plan bytes still carry the mapped address and that member's address.
#[test]
fn one_members_relay_only_hides_only_that_pair() {
    let mut world = listen();
    let Listen {
        hub,
        owner,
        member,
        open,
        lobby,
    } = &mut world;
    hub.offer_reach(*owner, *lobby, offer(false), 0).expect("offer");
    hub.ask_reach(*member, *lobby, ask(member_addr(), true), 0)
        .expect("hidden ask");
    hub.ask_reach(*open, *lobby, ask(open_addr(), false), 0)
        .expect("open ask");
    let hidden = hub
        .approve(*owner, *lobby, *member, true, 0)
        .expect("approve hidden")
        .expect("hidden plan");
    assert_rewritten(&hidden);
    let open_plan = hub
        .approve(*owner, *lobby, *open, true, 0)
        .expect("approve open")
        .expect("open plan");
    assert_eq!(open_plan.path, ReachKind::Mapped);
    assert_eq!(open_plan.mapped, Some(mapped()));
    assert!(!open_plan.candidates.is_empty());
    let hidden_member = plan_bytes(&hub.take_events(*member), *owner);
    let open_member = plan_bytes(&hub.take_events(*open), *owner);
    let owner_events = hub.take_events(*owner);
    let hidden_owner = plan_bytes(&owner_events, *member);
    let open_owner = plan_bytes(&owner_events, *open);
    assert!(!has_octets(&hidden_member, HOST));
    assert!(!has_octets(&hidden_member, MAPPED));
    assert!(!has_octets(&hidden_owner, MEMBER));
    assert!(has_octets(&open_member, MAPPED));
    assert!(has_octets(&open_member, HOST));
    assert!(has_octets(&open_owner, OPEN));
}

/// Relay-only with no relay and no join code does not fall through to a punch.
#[test]
fn relay_only_without_a_relay_stays_asked() {
    let mut world = listen();
    let Listen {
        hub,
        owner,
        member,
        lobby,
        ..
    } = &mut world;
    let mut bare = offer(true);
    bare.relay = None;
    bare.join_code = None;
    hub.offer_reach(*owner, *lobby, bare, 0).expect("offer");
    hub.ask_reach(*member, *lobby, ask(member_addr(), false), 0)
        .expect("ask");
    let err = hub
        .approve(*owner, *lobby, *member, true, 0)
        .expect_err("no path");
    assert_eq!(err, HubError::NoReach);
    assert!(
        hub.take_events(*member)
            .iter()
            .all(|event| !matches!(event, HubEvent::Plan(_)))
    );
    let row = hub
        .joins()
        .into_iter()
        .find(|row| row.member == *member)
        .expect("join row");
    assert_eq!(row.state, "asked");
    assert!(row.planned.is_none());
}

/// The flag clear: `Register` carries the host, and `PunchIntro` carries the member.
#[test]
fn register_and_punch_intro_carry_the_addresses_when_the_flag_is_clear() {
    let net = LoopbackNet::new();
    let log = Arc::new(Mutex::new(Vec::new()));
    let host_socket = tape(&net, host(), &log);
    let ticket = JoinTicket {
        addr: host(),
        server_key: Some(server_key()),
    };
    let mut registrar =
        RelayRegistrar::new(Box::new(host_socket), relay_addr(), &ticket).expect("ticket fits");
    registrar.update(0);
    let register: Vec<Sent> = log_of(&log);
    let registers = count_kind(&register, RelayTag::Register as u8);
    assert!(registers >= 1, "a Register went out");
    assert!(
        sensitive(&register)
            .iter()
            .any(|(kind, bytes)| *kind == RelayTag::Register as u8 && has_octets(bytes, HOST)),
        "the host address is in Register"
    );

    let member_socket = tape(&net, member_addr(), &log);
    let mut path = JoinPath::new(
        member_socket,
        relay_addr(),
        "ABCDEF",
        [9; RELAY_ALLOCATION_KEY_BYTES],
    )
    .expect("code");
    path.update(0);
    let intro = log_of(&log);
    assert!(
        sensitive(&intro)
            .iter()
            .any(|(kind, bytes)| *kind == RelayTag::PunchIntro as u8 && has_octets(bytes, MEMBER)),
        "the member address is in PunchIntro"
    );
}

/// `relay_only` rewrites a ticket that named the host, and a game-socket registrar announces no
/// candidate gathered from that bind address.
#[test]
fn relay_only_register_omits_the_host() {
    let net = LoopbackNet::new();
    let log = Arc::new(Mutex::new(Vec::new()));
    let ticket = JoinTicket {
        addr: host(),
        server_key: Some(server_key()),
    };
    let mut registrar = RelayRegistrar::new(Box::new(tape(&net, host(), &log)), relay_addr(), &ticket)
        .expect("ticket fits")
        .relay_only();
    registrar.update(0);
    let rows = log_of(&log);
    assert!(count_kind(&rows, RelayTag::Register as u8) >= 1);
    assert_eq!(count_kind(&rows, RelayTag::PunchIntro as u8), 0);
    assert_eq!(count_kind(&rows, CHECK), 0);
    assert_clean(&rows, &[HOST, MAPPED, MEMBER]);
    drop(registrar);

    let game = JoinTicket::from_game_socket(Some(server_key()));
    let mut gathered =
        RelayRegistrar::new(Box::new(tape(&net, host(), &log)), relay_addr(), &game).expect("ticket fits");
    gathered.update(0);
    let open = log_of(&log);
    assert!(
        sensitive(&open)
            .iter()
            .any(|(kind, bytes)| *kind == RelayTag::Register as u8 && has_octets(bytes, HOST)),
        "a game-socket Register announces the bind address"
    );

    drop(gathered);
    let mut hidden = RelayRegistrar::new(Box::new(tape(&net, host(), &log)), relay_addr(), &game)
        .expect("ticket fits")
        .relay_only();
    hidden.update(0);
    let hidden_rows = log_of(&log);
    assert!(count_kind(&hidden_rows, RelayTag::Register as u8) >= 1);
    assert_clean(&hidden_rows, &[HOST, MAPPED, MEMBER]);
}

/// The member path is `JoinPath::relay_first` through `HubReach`. Against a live relay it sends
/// `Register` and lookups, and it sends no `PunchIntro` and no connectivity check.
#[test]
fn relay_first_sends_no_introduction_and_no_checks() {
    let net = LoopbackNet::new();
    let log = Arc::new(Mutex::new(Vec::new()));
    let control = tape(&net, relay_addr(), &log);
    let pool = (0..4u16)
        .map(|i| tape(&net, addr(RELAY_IP, 50_000 + i), &log))
        .collect();
    let mut relay = RelayServer::from_core(
        control,
        pool,
        RelayCore::with_config(31, [9; 32], RelayConfig::default()),
    );
    let game = JoinTicket::from_game_socket(Some(server_key()));
    let mut registrar = RelayRegistrar::new(Box::new(tape(&net, host(), &log)), relay_addr(), &game)
        .expect("ticket fits")
        .relay_only();
    let mut now = 0u64;
    let mut code = None;
    for _ in 0..40 {
        registrar.update(now);
        relay.update(now);
        if let Some(got) = registrar.join_code() {
            code = Some(got.to_owned());
            break;
        }
        now += STEP_MS;
    }
    let code = code.unwrap_or_else(|| {
        panic!(
            "no join code (refusal {:?}, sent {})",
            registrar.last_refusal(),
            registrar.sent()
        )
    });
    let plan = JoinPlan {
        lobby: LobbyId(1),
        peer: MemberId(1),
        peer_identity: PlayerKey([1; 32]),
        path: ReachKind::Relay,
        mapped: None,
        candidates: Vec::new(),
        relay: Some(relay_addr()),
        join_code: Some(code),
        ticket: JoinTicket::from_game_socket(Some(server_key())).encode(),
    };
    let mut reach = HubReach::new(
        tape(&net, member_addr(), &log),
        &plan,
        [9; RELAY_ALLOCATION_KEY_BYTES],
    )
    .expect("relay plan");
    for _ in 0..80 {
        registrar.update(now);
        relay.update(now);
        reach.update(now);
        now += STEP_MS;
    }
    let rows = log_of(&log);
    assert!(registrar.sent() >= 1, "the host registered");
    assert!(count_kind(&rows, RelayTag::Register as u8) >= 1);
    assert!(
        count_kind(&rows, RelayTag::Lookup as u8) >= 1,
        "the member looked the code up"
    );
    assert_eq!(count_kind(&rows, RelayTag::PunchIntro as u8), 0);
    assert_eq!(count_kind(&rows, CHECK), 0);
    assert_clean(&rows, &[HOST, MAPPED, MEMBER, OPEN]);
}

fn payloads_of(rows: &[Sent], kind: u8) -> Vec<&[u8]> {
    rows.iter()
        .filter(|row| datagram_kind(&row.bytes) == Some(kind))
        .map(|row| row.bytes.as_slice())
        .collect()
}

fn offer_path(relay_only: bool, include_mapped: bool, nat: NatClass) -> ReachOffer {
    let mut candidates = vec![Candidate::new(CandidateKind::Host, host())];
    if include_mapped {
        candidates.push(Candidate::new(CandidateKind::Mapped, mapped()));
    }
    let mut offer = ReachOffer::default();
    offer.candidates = candidates;
    offer.nat = Some(nat);
    offer.relay = Some(relay_addr());
    offer.join_code = Some("ABCDEF".to_owned());
    offer.ticket = JoinTicket {
        addr: host(),
        server_key: Some(server_key()),
    }
    .encode();
    offer.relay_only = relay_only;
    offer
}

fn ask_nat(at: SocketAddr, relay_only: bool, nat: NatClass) -> ReachOffer {
    let mut offer = ReachOffer::default();
    offer.candidates = vec![Candidate::new(CandidateKind::Host, at)];
    offer.nat = Some(nat);
    offer.relay_only = relay_only;
    offer
}

/// One relay-only host and one `relay_first` member, driven until the allocation replies exist.
fn drive_hidden_pair() -> Vec<Sent> {
    let net = LoopbackNet::new();
    let log = Arc::new(Mutex::new(Vec::new()));
    let control = tape(&net, relay_addr(), &log);
    let pool = (0..4u16)
        .map(|i| tape(&net, addr(RELAY_IP, 50_000 + i), &log))
        .collect();
    let mut relay = RelayServer::from_core(
        control,
        pool,
        RelayCore::with_config(31, [9; 32], RelayConfig::default()),
    );
    let game = JoinTicket::from_game_socket(Some(server_key()));
    let mut registrar = RelayRegistrar::new(Box::new(tape(&net, host(), &log)), relay_addr(), &game)
        .expect("ticket fits")
        .relay_only();
    let mut now = 0u64;
    let mut code = None;
    for _ in 0..40 {
        registrar.update(now);
        relay.update(now);
        if let Some(got) = registrar.join_code() {
            code = Some(got.to_owned());
            break;
        }
        now += STEP_MS;
    }
    let code = code.expect("join code");
    let plan = JoinPlan {
        lobby: LobbyId(1),
        peer: MemberId(1),
        peer_identity: PlayerKey([1; 32]),
        path: ReachKind::Relay,
        mapped: None,
        candidates: Vec::new(),
        relay: Some(relay_addr()),
        join_code: Some(code),
        ticket: JoinTicket::from_game_socket(Some(server_key())).encode(),
    };
    let mut reach = HubReach::new(
        tape(&net, member_addr(), &log),
        &plan,
        [9; RELAY_ALLOCATION_KEY_BYTES],
    )
    .expect("relay plan");
    for _ in 0..80 {
        registrar.update(now);
        relay.update(now);
        reach.update(now);
        now += STEP_MS;
    }
    log_of(&log)
}

/// `Lookup`'s `HostInfo`, the member's `Allocate`, and the relay's `Allocated` and
/// `ClientJoined` are bytes a party receives. None of them may name the other side.
#[test]
fn relay_only_replies_must_not_name_either_side() {
    let rows = drive_hidden_pair();
    assert_eq!(count_kind(&rows, RelayTag::PunchIntro as u8), 0);
    assert_eq!(count_kind(&rows, CHECK), 0);
    let checks = [
        ("HostInfo", RelayTag::HostInfo as u8, HOST, "the member"),
        (
            "Allocate",
            RelayTag::Allocate as u8,
            HOST,
            "the relay, from the member",
        ),
        ("Allocated", RelayTag::Allocated as u8, HOST, "the member"),
        ("ClientJoined", RelayTag::ClientJoined as u8, MEMBER, "the host"),
    ];
    let mut leaks = Vec::new();
    for (name, kind, ip, who) in checks {
        let found = payloads_of(&rows, kind);
        if found.is_empty() {
            leaks.push(format!("{name} was not sent"));
            continue;
        }
        if found.iter().any(|bytes| has_octets(bytes, ip)) {
            leaks.push(format!("{name} carried {ip:?} to {who}"));
        }
    }
    assert!(
        leaks.is_empty(),
        "relay-only replies named the other side: {leaks:?}"
    );
}

/// The server's stored `relay_only` applies to every member. The offer itself leaves the flag
/// clear and still has a mapped candidate, so a planner that ignores the room flag plans mapped
/// and copies both addresses.
#[test]
fn server_flag_hides_both_plan_directions() {
    let game = ServerIdentity::from_private([0x71; 32]).expect("curve");
    let hub_key = ServerIdentity::from_private([0x41; 32]).expect("curve");
    let mut hub = HubServer::with_secret(HubSettings::default(), [3; 32]);
    hub.set_hub_identity(hub_key.clone());
    let hash = [0x5A; 32];
    let server = hub.connect("srv", 0).expect("server");
    hub.note_link_hash(server, hash);
    hub.set_wire(server, 2);
    let presence = ServerPresence {
        server_proof: prove_server_presence(&game, &hub_key.public(), &hash).expect("proof"),
        proofs: Vec::new(),
        capacity: 8,
        load: 0,
        protocol: 1,
        game_version: "2".into(),
        relay_only: true,
        grace_ms: 0,
        bans: Vec::new(),
    };
    hub.register_server(server, &presence, 0).expect("register");
    let lobby = hub.create(server, &LobbySpec::default(), 0).expect("lobby");
    let member = hub
        .connect_player(PlayerKey([2; 32]), "member", 0)
        .expect("member");
    hub.set_wire(member, 2);
    hub.join(member, lobby, None, 0).expect("join");
    let stored = hub
        .take_events(member)
        .into_iter()
        .any(|event| matches!(event, HubEvent::Room(room) if room.relay_only));
    assert!(
        stored,
        "the server flag is stored on the room the member receives"
    );
    hub.offer_reach(server, lobby, offer(false), 0).expect("offer");
    hub.ask_reach(member, lobby, ask(member_addr(), false), 0)
        .expect("ask");
    let plan = hub
        .approve(server, lobby, member, true, 0)
        .expect("approve")
        .expect("a plan");
    assert_rewritten(&plan);
    let member_bytes = plan_bytes(&hub.take_events(member), server);
    let owner_bytes = plan_bytes(&hub.take_events(server), member);
    assert!(!has_octets(&member_bytes, HOST));
    assert!(!has_octets(&member_bytes, MAPPED));
    assert!(!has_octets(&owner_bytes, MEMBER));
}

/// `SetReachMode(true)` on a listen lobby hides the next exchange even when neither offer sets
/// the flag. Clearing it afterwards is the same call with `false`.
#[test]
fn listen_lobby_flag_hides_the_exchange() {
    let mut world = listen();
    let Listen {
        hub,
        owner,
        member,
        lobby,
        ..
    } = &mut world;
    set_reach_mode(hub, *owner, *lobby, true, 0).unwrap_or_else(|err| {
        panic!("SetReachMode must store the lobby flag, got {err:?}");
    });
    hub.offer_reach(*owner, *lobby, offer(false), 0).expect("offer");
    hub.ask_reach(*member, *lobby, ask(member_addr(), false), 0)
        .expect("ask");
    let plan = hub
        .approve(*owner, *lobby, *member, true, 0)
        .expect("approve")
        .expect("a plan");
    assert_rewritten(&plan);
    let member_bytes = plan_bytes(&hub.take_events(*member), *owner);
    let owner_bytes = plan_bytes(&hub.take_events(*owner), *member);
    assert!(!has_octets(&member_bytes, HOST));
    assert!(!has_octets(&member_bytes, MAPPED));
    assert!(!has_octets(&owner_bytes, MEMBER));
}

/// A relay-only ask, from either side, must not deliver the member's address to the host.
#[test]
fn relay_only_ask_must_not_name_the_member() {
    let mut leaks = Vec::new();
    for (owner_flag, member_flag) in [(true, false), (false, true)] {
        let mut world = listen();
        let Listen {
            hub,
            owner,
            member,
            lobby,
            ..
        } = &mut world;
        hub.offer_reach(*owner, *lobby, offer(owner_flag), 0)
            .expect("offer");
        hub.ask_reach(*member, *lobby, ask(member_addr(), member_flag), 0)
            .expect("ask");
        let ask_event = hub.take_events(*owner).into_iter().find_map(|event| match event {
            HubEvent::ReachAsk { .. } => Some(event),
            _ => None,
        });
        let Some(ask_event) = ask_event else {
            leaks.push(format!(
                "owner flag {owner_flag}, member flag {member_flag}: no ReachAsk"
            ));
            continue;
        };
        let bytes = ask_event.to_bytes().expect("ask bytes");
        if has_octets(&bytes, MEMBER) {
            leaks.push(format!(
                "owner flag {owner_flag}, member flag {member_flag}: ReachAsk carried the member address"
            ));
        }
    }
    assert!(leaks.is_empty(), "ReachAsk named the member: {leaks:?}");
}

fn exchange(
    hub: &mut HubServer,
    owner: MemberId,
    member: MemberId,
    lobby: LobbyId,
    ask_at: SocketAddr,
    ask_flag: bool,
    ask_class: NatClass,
) -> (JoinPlan, Vec<u8>, Vec<u8>) {
    hub.ask_reach(member, lobby, ask_nat(ask_at, ask_flag, ask_class), 0)
        .expect("ask");
    let plan = hub
        .approve(owner, lobby, member, true, 0)
        .expect("approve")
        .expect("a plan");
    let member_bytes = plan_bytes(&hub.take_events(member), owner);
    let owner_bytes = plan_bytes(&hub.take_events(owner), member);
    (plan, member_bytes, owner_bytes)
}

/// An offer that sets the flag, then a later offer that clears it, plans mapped, then punch,
/// then relay again. The cleared relay plan still carries the addresses. The hidden plan does not.
#[test]
fn clearing_the_offer_flag_restores_mapped_punch_and_relay() {
    let mut world = listen();
    let Listen {
        hub,
        owner,
        member,
        lobby,
        ..
    } = &mut world;
    hub.offer_reach(*owner, *lobby, offer_path(true, true, NatClass::FULL_CONE), 0)
        .expect("hide");
    let (hidden, hidden_member, hidden_owner) = exchange(
        hub,
        *owner,
        *member,
        *lobby,
        member_addr(),
        false,
        NatClass::PORT_RESTRICTED,
    );
    assert_eq!(hidden.path, ReachKind::Relay);
    assert!(!has_octets(&hidden_member, HOST));
    assert!(!has_octets(&hidden_member, MAPPED));
    assert!(!has_octets(&hidden_owner, MEMBER));

    hub.offer_reach(*owner, *lobby, offer_path(false, true, NatClass::FULL_CONE), 0)
        .expect("clear to mapped");
    let (mapped_plan, mapped_member, mapped_owner) = exchange(
        hub,
        *owner,
        *member,
        *lobby,
        member_addr(),
        false,
        NatClass::PORT_RESTRICTED,
    );
    assert_eq!(mapped_plan.path, ReachKind::Mapped);
    assert_eq!(mapped_plan.mapped, Some(mapped()));
    assert!(has_octets(&mapped_member, MAPPED));
    assert!(has_octets(&mapped_member, HOST));
    assert!(has_octets(&mapped_owner, MEMBER));

    hub.offer_reach(*owner, *lobby, offer_path(false, false, NatClass::FULL_CONE), 0)
        .expect("clear to punch");
    let (punch_plan, punch_member, punch_owner) = exchange(
        hub,
        *owner,
        *member,
        *lobby,
        member_addr(),
        false,
        NatClass::PORT_RESTRICTED,
    );
    assert_eq!(punch_plan.path, ReachKind::Punch);
    assert!(has_octets(&punch_member, HOST));
    assert!(has_octets(&punch_owner, MEMBER));

    hub.offer_reach(*owner, *lobby, offer_path(false, false, NatClass::SYMMETRIC), 0)
        .expect("clear to relay");
    let (relay_plan, relay_member, relay_owner) = exchange(
        hub,
        *owner,
        *member,
        *lobby,
        member_addr(),
        false,
        NatClass::SYMMETRIC,
    );
    assert_eq!(relay_plan.path, ReachKind::Relay);
    assert!(!relay_plan.candidates.is_empty());
    assert!(has_octets(&relay_member, HOST));
    assert!(has_octets(&relay_owner, MEMBER));
}

/// `ReachDone` and the owner's `Reached` event carry a path and a count, not an address.
#[test]
fn reach_done_bytes_carry_no_address() {
    let mut world = listen();
    let Listen {
        hub,
        owner,
        member,
        lobby,
        ..
    } = &mut world;
    hub.offer_reach(*owner, *lobby, offer(true), 0).expect("offer");
    hub.ask_reach(*member, *lobby, ask(member_addr(), false), 0)
        .expect("ask");
    hub.approve(*owner, *lobby, *member, true, 0)
        .expect("approve")
        .expect("a plan");
    let _ = hub.take_events(*owner);
    let _ = hub.take_events(*member);
    hub.reach_done(*member, *lobby, ReachKind::Relay, 0, 0)
        .expect("done");
    let reached = hub
        .take_events(*owner)
        .into_iter()
        .find(|event| matches!(event, HubEvent::Reached { .. }))
        .expect("Reached");
    let event_bytes = reached.to_bytes().expect("event bytes");
    let request_bytes = HubRequest::ReachDone {
        lobby: *lobby,
        path: ReachKind::Relay,
        punch_attempts: 4,
    }
    .to_bytes()
    .expect("request bytes");
    for bytes in [event_bytes.as_slice(), request_bytes.as_slice()] {
        assert!(!has_octets(bytes, HOST));
        assert!(!has_octets(bytes, MAPPED));
        assert!(!has_octets(bytes, MEMBER));
    }
}
