// SPDX-License-Identifier: MPL-2.0
//! Acceptance for the three helpers DO27 kept because `regia_net` lacked them
//! (spec 120 § 14, rows N4 and N8, plan N232).
//!
//! Time is a step counter. Address-code expiry takes the caller's unix second.
//! Nothing sleeps and nothing reads the wall clock.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use regia_net::LoopbackSocket;
use regia_net::address_join::{
    ADDRESS_JOIN_TTL_DEFAULT_S, AddressJoinError, decode_address_join, encode_address_join, mint_address_join,
};
use regia_net::discovery::{
    ANNOUNCE_MAGIC, DISCOVERY_PORT, DiscoveryConfig, DiscoveryHub, LanAnnouncer, ServerAnnounce,
};
use regia_net::join_code::{JOIN_CODE_ALPHABET, mint_join_code};
use regia_net::prelude::*;
use regia_net::ticket::{JoinTicketCheck, TicketRefused, TicketVerdict};

const STEP_MS: u64 = 10;

fn addr(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

fn announce(name: &str, game_port: u16) -> ServerAnnounce {
    ServerAnnounce {
        protocol_id: 1,
        game_port,
        players: 1,
        max_players: 8,
        password: false,
        name: name.into(),
        card: Vec::new(),
    }
}

fn names_of(heard: &[ServerAnnounce]) -> Vec<&str> {
    heard.iter().map(|row| row.name.as_str()).collect()
}

/// Two announcers, one port, two magics. Each hears itself and not the other.
#[test]
fn two_announcers_with_different_magics_on_one_port_do_not_see_each_other() {
    let hub = DiscoveryHub::new();
    let mut alpha = LanAnnouncer::bind(
        hub.clone(),
        DiscoveryConfig {
            magic: *b"RGNA",
            port: DISCOVERY_PORT,
        },
    )
    .expect("bind alpha");
    let mut beta = LanAnnouncer::bind(
        hub,
        DiscoveryConfig {
            magic: *b"IRNR",
            port: DISCOVERY_PORT,
        },
    )
    .expect("bind beta");
    alpha.publish(&announce("alpha", 7777)).expect("publish alpha");
    beta.publish(&announce("beta", 7778)).expect("publish beta");

    let polled_alpha = alpha.poll();
    let polled_beta = beta.poll();
    let heard_alpha = names_of(&polled_alpha);
    let heard_beta = names_of(&polled_beta);
    assert!(
        heard_alpha.contains(&"alpha"),
        "alpha hears its own announce, got {heard_alpha:?}"
    );
    assert!(
        !heard_alpha.contains(&"beta"),
        "alpha drops a foreign magic, got {heard_alpha:?}"
    );
    assert!(
        heard_beta.contains(&"beta"),
        "beta hears its own announce, got {heard_beta:?}"
    );
    assert!(
        !heard_beta.contains(&"alpha"),
        "beta drops a foreign magic, got {heard_beta:?}"
    );
}

/// Same magic on one port is a positive control: each side hears both names.
#[test]
fn two_announcers_with_the_same_magic_on_one_port_see_each_other() {
    let hub = DiscoveryHub::new();
    let cfg = DiscoveryConfig {
        magic: *b"RGNA",
        port: DISCOVERY_PORT,
    };
    let mut alpha = LanAnnouncer::bind(hub.clone(), cfg).expect("bind alpha");
    let mut beta = LanAnnouncer::bind(hub, cfg).expect("bind beta");
    alpha.publish(&announce("alpha", 7777)).expect("publish alpha");
    beta.publish(&announce("beta", 7778)).expect("publish beta");
    let polled_alpha = alpha.poll();
    let polled_beta = beta.poll();
    let heard_alpha = names_of(&polled_alpha);
    let heard_beta = names_of(&polled_beta);
    assert!(heard_alpha.contains(&"alpha") && heard_alpha.contains(&"beta"));
    assert!(heard_beta.contains(&"alpha") && heard_beta.contains(&"beta"));
}

/// A different port is a different bus, even with the same magic.
#[test]
fn announcers_on_different_ports_do_not_see_each_other() {
    let hub = DiscoveryHub::new();
    let mut alpha = LanAnnouncer::bind(
        hub.clone(),
        DiscoveryConfig {
            magic: ANNOUNCE_MAGIC,
            port: DISCOVERY_PORT,
        },
    )
    .expect("bind alpha");
    let mut beta = LanAnnouncer::bind(
        hub,
        DiscoveryConfig {
            magic: ANNOUNCE_MAGIC,
            port: DISCOVERY_PORT + 1,
        },
    )
    .expect("bind beta");
    alpha.publish(&announce("alpha", 7777)).expect("publish alpha");
    beta.publish(&announce("beta", 7778)).expect("publish beta");
    assert_eq!(names_of(&alpha.poll()), vec!["alpha"]);
    assert_eq!(names_of(&beta.poll()), vec!["beta"]);
}

/// Defaults stay the historical magic and port. `encode` / `decode` stay on that magic.
#[test]
fn discovery_defaults_stay_rgna_on_udp_7900() {
    let cfg = DiscoveryConfig::default();
    assert_eq!(cfg.magic, ANNOUNCE_MAGIC);
    assert_eq!(cfg.magic, *b"RGNA");
    assert_eq!(cfg.port, DISCOVERY_PORT);
    assert_eq!(cfg.port, 7900);
    let row = announce("friday", 7777);
    let bytes = row.encode().expect("fits");
    assert_eq!(ServerAnnounce::decode(&bytes), Ok(Some(row.clone())));
    let foreign = row.encode_with(b"IRNR").expect("fits");
    assert_eq!(ServerAnnounce::decode(&foreign), Ok(None));
    assert_eq!(ServerAnnounce::decode_with(&foreign, b"IRNR"), Ok(Some(row)));
}

/// An address code round-trips and is expired exactly at its TTL.
#[test]
fn address_code_round_trips_and_expires_at_its_ttl() {
    let addr: SocketAddr = "192.0.2.10:7777".parse().expect("addr");
    let now = 1_700_000_000u64;
    let nonce = 0x0123_4567_89AB_CDEF;
    let ttl = ADDRESS_JOIN_TTL_DEFAULT_S;
    assert_eq!(ttl, 12 * 60 * 60, "the default TTL is 12 hours");
    let minted = mint_address_join(addr, nonce, now, ttl);
    let code = encode_address_join(&minted);
    assert!(
        code.chars().all(|ch| JOIN_CODE_ALPHABET.contains(&(ch as u8))),
        "the code uses the typo-resistant alphabet: {code}"
    );
    let still = decode_address_join(&code, now + ttl - 1).expect("valid one second before expiry");
    assert_eq!(still.addr, addr);
    assert_eq!(still.nonce, nonce);
    assert_eq!(still.expires_at_unix_s, now + ttl);
    assert!(
        matches!(
            decode_address_join(&code, now + ttl),
            Err(AddressJoinError::Expired { .. })
        ),
        "expired at the TTL instant"
    );
}

/// IPv6 round-trips, a relay code is not an address code, a one-character typo fails the
/// checksum, and lowercase or dashes still decode.
#[test]
fn address_code_ipv6_typo_relay_and_typing() {
    let now = 1_700_000_000u64;
    let v6: SocketAddr = "[2001:db8::1]:27015".parse().expect("v6");
    let minted = mint_address_join(v6, 99, now, 60);
    let code = encode_address_join(&minted);
    let back = decode_address_join(&code, now).expect("v6 round trip");
    assert_eq!(back.addr, v6);
    assert_eq!(back.nonce, 99);

    let relay = mint_join_code(1);
    assert!(
        matches!(
            decode_address_join(&relay, now),
            Err(AddressJoinError::NotAnAddressCode)
        ),
        "a 6-character relay code is not an address code"
    );

    let mut chars: Vec<u8> = code.bytes().collect();
    let replacement = JOIN_CODE_ALPHABET
        .iter()
        .copied()
        .find(|ch| *ch != chars[0])
        .expect("alphabet has another character");
    chars[0] = replacement;
    let typo = String::from_utf8(chars).expect("ascii");
    assert!(
        matches!(decode_address_join(&typo, now), Err(AddressJoinError::Checksum)),
        "one changed character fails the checksum"
    );

    let mut typed = String::new();
    for (i, ch) in code.chars().enumerate() {
        if i > 0 && i.is_multiple_of(4) {
            typed.push('-');
        }
        typed.push(ch.to_ascii_lowercase());
    }
    let loose = decode_address_join(&typed, now).expect("dashes and lowercase");
    assert_eq!(loose.addr, v6);
    assert_eq!(loose.nonce, 99);
}

/// A join whose ticket the hook refuses ends as `ticket_refused` and is not admitted.
#[test]
fn join_with_a_refused_ticket_is_denied_ticket_refused() {
    let net = LoopbackNet::new();
    let mut server = NetServer::<TicketGame, _>::with_secret(
        net.bind(addr(7777)).expect("bind server"),
        ServerConfig::default(),
        [9; 32],
    );
    let calls = Arc::new(AtomicUsize::new(0));
    server.set_ticket_check(Refuse {
        calls: Arc::clone(&calls),
        accept: b"other",
    });
    let mut client = NetClient::<TicketGame, _>::connect(
        net.bind(addr(0)).expect("bind client"),
        addr(7777),
        ClientConfig {
            name: "ada".into(),
            ..ClientConfig::default()
        },
        &b"store-ticket".to_vec(),
    )
    .expect("join fits");
    let mut now = 0u64;
    for _ in 0..100 {
        now += STEP_MS;
        client.update(now);
        server.update(now);
    }
    match client.state() {
        ClientState::Ended(end) => {
            assert_eq!(end.as_str(), TicketRefused::CODE);
            assert_eq!(end.as_str(), "ticket_refused");
            match end {
                ClientEnd::Denied { reason, detail } => {
                    assert_eq!(*reason, DenyReason::InvalidJoin);
                    assert_eq!(detail, "ticket_refused");
                }
                other => panic!("expected a denial, got {other:?}"),
            }
        }
        other => panic!("expected the client to have ended, got {other:?}"),
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1, "the hook ran once");
    assert!(server.peer_rows().is_empty(), "a refused ticket is not admitted");
}

/// The host password is the access control. A wrong password never reaches the ticket hook.
#[test]
fn wrong_password_does_not_call_the_ticket_hook() {
    let net = LoopbackNet::new();
    let mut server = NetServer::<TicketGame, _>::with_secret(
        net.bind(addr(7777)).expect("bind server"),
        ServerConfig {
            password: Some("sesame".into()),
            ..ServerConfig::default()
        },
        [9; 32],
    );
    let calls = Arc::new(AtomicUsize::new(0));
    server.set_ticket_check(Refuse {
        calls: Arc::clone(&calls),
        accept: b"store-ticket",
    });
    let mut client = NetClient::<TicketGame, _>::connect(
        net.bind(addr(0)).expect("bind client"),
        addr(7777),
        ClientConfig {
            name: "ada".into(),
            password: Some("nope".into()),
            ..ClientConfig::default()
        },
        &b"store-ticket".to_vec(),
    )
    .expect("join fits");
    let mut now = 0u64;
    for _ in 0..100 {
        now += STEP_MS;
        client.update(now);
        server.update(now);
    }
    match client.state() {
        ClientState::Ended(end) => assert_eq!(end.as_str(), "bad_password"),
        other => panic!("expected a password denial, got {other:?}"),
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(server.peer_rows().is_empty());
}

/// No hook leaves admission unchanged, and a hook that accepts admits the peer.
#[test]
fn no_hook_admits_and_an_accepting_hook_admits() {
    let net = LoopbackNet::new();
    let mut open = NetServer::<TicketGame, _>::with_secret(
        net.bind(addr(7777)).expect("bind open"),
        ServerConfig::default(),
        [3; 32],
    );
    let mut plain = NetClient::<TicketGame, _>::connect(
        net.bind(addr(0)).expect("bind plain"),
        addr(7777),
        ClientConfig {
            name: "plain".into(),
            ..ClientConfig::default()
        },
        &b"anything".to_vec(),
    )
    .expect("join fits");

    let mut gated = NetServer::<TicketGame, _>::with_secret(
        net.bind(addr(7778)).expect("bind gated"),
        ServerConfig::default(),
        [4; 32],
    );
    gated.set_ticket_check(Refuse {
        calls: Arc::new(AtomicUsize::new(0)),
        accept: b"good",
    });
    let mut good = NetClient::<TicketGame, _>::connect(
        net.bind(addr(0)).expect("bind good"),
        addr(7778),
        ClientConfig {
            name: "good".into(),
            ..ClientConfig::default()
        },
        &b"good".to_vec(),
    )
    .expect("join fits");

    let mut now = 0u64;
    for _ in 0..100 {
        now += STEP_MS;
        plain.update(now);
        good.update(now);
        open.update(now);
        gated.update(now);
    }
    assert!(plain.is_connected(), "no hook does not deny a ticket");
    assert!(good.is_connected(), "an accepting hook admits");
    assert_eq!(open.peer_rows().len(), 1);
    assert_eq!(gated.peer_rows().len(), 1);
}

/// Game whose join data is the store ticket bytes.
struct TicketGame;

impl NetGame for TicketGame {
    type Message = ();
    type Input = ();
    type JoinData = Vec<u8>;
    const GAME_ID: &'static str = "regia.test.ticket";
    const PROTOCOL_VERSION: u32 = 1;

    fn join_ticket(data: &Vec<u8>) -> &[u8] {
        data
    }
}

/// One monotonic clock for a server and the clients on its loopback network.
fn drive<S: DatagramSocket>(
    now: &mut u64,
    steps: u32,
    server: &mut NetServer<TicketGame, S>,
    clients: &mut [&mut NetClient<TicketGame, S>],
) {
    for _ in 0..steps {
        *now += STEP_MS;
        for client in clients.iter_mut() {
            client.update(*now);
        }
        server.update(*now);
    }
}

/// Counts an open store session per ticket. A second check of a ticket that was never ended is
/// `DuplicateRequest`, which this hook reports as [`TicketRefused`].
struct OpenSessions {
    open: Mutex<HashSet<Vec<u8>>>,
}

impl OpenSessions {
    fn new() -> Self {
        Self {
            open: Mutex::new(HashSet::new()),
        }
    }
}

impl JoinTicketCheck for OpenSessions {
    fn check_ticket(&self, ticket: &[u8]) -> Result<(), TicketRefused> {
        let mut open = self
            .open
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !open.insert(ticket.to_vec()) {
            return Err(TicketRefused);
        }
        Ok(())
    }

    fn end_ticket(&self, ticket: &[u8]) {
        self.open
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(ticket);
    }
}

/// A player who leaves and comes back with the same ticket is admitted again.
#[test]
fn a_player_who_leaves_can_rejoin_with_the_same_ticket() {
    let net = LoopbackNet::new();
    let mut server = NetServer::<TicketGame, _>::with_secret(
        net.bind(addr(7777)).expect("bind server"),
        ServerConfig::default(),
        [11; 32],
    );
    server.set_ticket_check(OpenSessions::new());
    let mut now = 0u64;
    let mut first = NetClient::<TicketGame, _>::connect(
        net.bind(addr(0)).expect("bind first"),
        addr(7777),
        ClientConfig {
            name: "ada".into(),
            ..ClientConfig::default()
        },
        &b"ticket-ada".to_vec(),
    )
    .expect("join fits");
    drive(&mut now, 100, &mut server, &mut [&mut first]);
    assert!(
        first.is_connected(),
        "the first join is admitted, state {:?}",
        first.state()
    );
    first.disconnect();
    drive(&mut now, 20, &mut server, &mut [&mut first]);
    assert!(server.peer_rows().is_empty(), "the peer left");
    let mut second = NetClient::<TicketGame, _>::connect(
        net.bind(addr(0)).expect("bind second"),
        addr(7777),
        ClientConfig {
            name: "ada".into(),
            ..ClientConfig::default()
        },
        &b"ticket-ada".to_vec(),
    )
    .expect("join fits");
    drive(&mut now, 100, &mut server, &mut [&mut second]);
    assert!(
        second.is_connected(),
        "a rejoin after leave is admitted, state {:?}",
        second.state()
    );
}

/// A join denied because the server is full does not leave a store session open, so the same
/// ticket is accepted once a slot is free.
#[test]
fn a_player_denied_because_the_server_is_full_can_join_once_a_slot_opens() {
    let net = LoopbackNet::new();
    let mut server = NetServer::<TicketGame, _>::with_secret(
        net.bind(addr(7780)).expect("bind server"),
        ServerConfig {
            max_peers: 1,
            ..ServerConfig::default()
        },
        [12; 32],
    );
    server.set_ticket_check(OpenSessions::new());
    let mut now = 0u64;
    let mut holder = NetClient::<TicketGame, _>::connect(
        net.bind(addr(0)).expect("bind holder"),
        addr(7780),
        ClientConfig {
            name: "holder".into(),
            ..ClientConfig::default()
        },
        &b"holder".to_vec(),
    )
    .expect("join fits");
    drive(&mut now, 100, &mut server, &mut [&mut holder]);
    assert!(
        holder.is_connected(),
        "the only slot is taken, state {:?}",
        holder.state()
    );

    let mut blocked = NetClient::<TicketGame, _>::connect(
        net.bind(addr(0)).expect("bind blocked"),
        addr(7780),
        ClientConfig {
            name: "waiter".into(),
            ..ClientConfig::default()
        },
        &b"waiter".to_vec(),
    )
    .expect("join fits");
    drive(&mut now, 100, &mut server, &mut [&mut holder, &mut blocked]);
    match blocked.state() {
        ClientState::Ended(end) => assert_eq!(
            end.as_str(),
            "full",
            "a full server denies with full, got {end:?}"
        ),
        other => panic!("expected a full denial, got {other:?}"),
    }

    holder.disconnect();
    drive(&mut now, 20, &mut server, &mut [&mut holder]);
    assert!(server.peer_rows().is_empty(), "the slot opened");

    let mut retry = NetClient::<TicketGame, _>::connect(
        net.bind(addr(0)).expect("bind retry"),
        addr(7780),
        ClientConfig {
            name: "waiter".into(),
            ..ClientConfig::default()
        },
        &b"waiter".to_vec(),
    )
    .expect("join fits");
    drive(&mut now, 100, &mut server, &mut [&mut retry]);
    assert!(
        retry.is_connected(),
        "a retry after a free slot is admitted, state {:?}",
        retry.state()
    );
}

/// Polling removes datagrams. A listener that polls can publish past the old 1,024 cap.
#[test]
fn polling_an_announcer_lets_publish_pass_the_old_queue_cap() {
    let hub = DiscoveryHub::new();
    let mut announcer = LanAnnouncer::bind(hub, DiscoveryConfig::default()).expect("bind");
    for i in 0..1025u32 {
        let name = format!("n{i}");
        if let Err(err) = announcer.publish(&announce(&name, 1)) {
            panic!("publish {i} failed: {err}");
        }
        let heard = announcer.poll();
        assert_eq!(heard.len(), 1, "step {i} heard {}", heard.len());
        assert_eq!(heard[0].name, name);
    }
}

/// A store that admits as [`TicketVerdict::Pending`] and can refuse the ticket later.
struct LateStore {
    inner: Arc<Mutex<LateInner>>,
}

struct LateInner {
    open: HashSet<Vec<u8>>,
    refused: Vec<Vec<u8>>,
}

struct LateHandle {
    inner: Arc<Mutex<LateInner>>,
}

impl LateStore {
    fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(LateInner {
                open: HashSet::new(),
                refused: Vec::new(),
            })),
        }
    }

    fn handle(&self) -> LateHandle {
        LateHandle {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl LateHandle {
    fn refuse(&self, ticket: &[u8]) {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .refused
            .push(ticket.to_vec());
    }
}

fn late_lock(inner: &Mutex<LateInner>) -> std::sync::MutexGuard<'_, LateInner> {
    inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl JoinTicketCheck for LateStore {
    fn check_ticket(&self, ticket: &[u8]) -> Result<(), TicketRefused> {
        match self.verdict(ticket) {
            TicketVerdict::Refuse => Err(TicketRefused),
            TicketVerdict::Admit | TicketVerdict::Pending => Ok(()),
        }
    }

    fn verdict(&self, ticket: &[u8]) -> TicketVerdict {
        let mut inner = late_lock(&self.inner);
        if !inner.open.insert(ticket.to_vec()) {
            return TicketVerdict::Refuse;
        }
        TicketVerdict::Pending
    }

    fn end_ticket(&self, ticket: &[u8]) {
        let mut inner = late_lock(&self.inner);
        inner.open.remove(ticket);
        inner.refused.retain(|row| row != ticket);
    }

    fn take_refusals(&self) -> Vec<Vec<u8>> {
        std::mem::take(&mut late_lock(&self.inner).refused)
    }
}

fn connect_player(
    net: &LoopbackNet,
    port: u16,
    name: &str,
    ticket: &[u8],
) -> NetClient<TicketGame, LoopbackSocket> {
    NetClient::connect(
        net.bind(addr(0)).expect("bind client"),
        addr(port),
        ClientConfig {
            name: name.into(),
            ..ClientConfig::default()
        },
        &ticket.to_vec(),
    )
    .expect("join fits")
}

fn joined_peer(server: &mut NetServer<TicketGame, LoopbackSocket>) -> PeerId {
    let mut found = None;
    while let Some(event) = server.poll_event() {
        if let ServerEvent::PeerJoined { peer, .. } = event {
            found = Some(peer);
        }
    }
    found.expect("the server recorded a join")
}

/// A pending ticket that the store refuses later kicks the peer. The same ticket can rejoin.
#[test]
fn a_late_store_refusal_kicks_the_peer_and_a_rejoin_is_admitted() {
    let net = LoopbackNet::new();
    let mut server = NetServer::<TicketGame, _>::with_secret(
        net.bind(addr(7791)).expect("bind server"),
        ServerConfig::default(),
        [13; 32],
    );
    let hook = LateStore::new();
    let handle = hook.handle();
    server.set_ticket_check(hook);
    let mut now = 0u64;
    let ticket = b"ticket-late".to_vec();
    let mut first = connect_player(&net, 7791, "ada", &ticket);
    drive(&mut now, 100, &mut server, &mut [&mut first]);
    assert!(
        first.is_connected(),
        "a pending verdict admits, state {:?}",
        first.state()
    );
    handle.refuse(&ticket);
    drive(&mut now, 20, &mut server, &mut [&mut first]);
    match first.state() {
        ClientState::Ended(end) => assert_eq!(end.as_str(), "kicked", "a late refusal kicks, got {end:?}"),
        other => panic!("expected a kick, got {other:?}"),
    }
    assert!(server.peer_rows().is_empty(), "the kicked peer is gone");
    let mut second = connect_player(&net, 7791, "ada", &ticket);
    drive(&mut now, 100, &mut server, &mut [&mut second]);
    assert!(
        second.is_connected(),
        "a rejoin after the kick is admitted, state {:?}",
        second.state()
    );
}

/// `finish_ticket(false)` is the host's kick for a pending peer, and the ticket can be used again.
#[test]
fn finish_ticket_false_kicks_and_the_ticket_can_rejoin() {
    let net = LoopbackNet::new();
    let mut server = NetServer::<TicketGame, _>::with_secret(
        net.bind(addr(7792)).expect("bind server"),
        ServerConfig::default(),
        [14; 32],
    );
    server.set_ticket_check(LateStore::new());
    let mut now = 0u64;
    let ticket = b"ticket-finish".to_vec();
    let mut first = connect_player(&net, 7792, "bea", &ticket);
    drive(&mut now, 100, &mut server, &mut [&mut first]);
    assert!(first.is_connected(), "pending admit, state {:?}", first.state());
    let peer = joined_peer(&mut server);
    assert!(server.finish_ticket(peer, false), "the pending peer is kicked");
    assert!(
        !server.finish_ticket(peer, false),
        "a second finish finds no peer"
    );
    drive(&mut now, 20, &mut server, &mut [&mut first]);
    match first.state() {
        ClientState::Ended(end) => assert_eq!(end.as_str(), "kicked", "finish_ticket kicks, got {end:?}"),
        other => panic!("expected a kick, got {other:?}"),
    }
    let mut second = connect_player(&net, 7792, "bea", &ticket);
    drive(&mut now, 100, &mut server, &mut [&mut second]);
    assert!(
        second.is_connected(),
        "a rejoin after finish_ticket is admitted, state {:?}",
        second.state()
    );
}

/// Refuses every ticket except `accept`, and counts calls.
struct Refuse {
    calls: Arc<AtomicUsize>,
    accept: &'static [u8],
}

impl JoinTicketCheck for Refuse {
    fn check_ticket(&self, ticket: &[u8]) -> Result<(), TicketRefused> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if ticket == self.accept {
            Ok(())
        } else {
            Err(TicketRefused)
        }
    }
}

/// Review net-rev-b #11: a host on the ECS loop calls `receive` and `flush`, never `update`. A
/// late store refusal must still kick the peer there.
#[test]
fn a_late_refusal_kicks_on_a_receive_and_flush_loop() {
    let net = LoopbackNet::new();
    let mut server = NetServer::<TicketGame, _>::with_secret(
        net.bind(addr(7793)).expect("bind server"),
        ServerConfig::default(),
        [15; 32],
    );
    let hook = LateStore::new();
    let handle = hook.handle();
    server.set_ticket_check(hook);
    let ticket = b"ticket-ecs-loop".to_vec();
    let mut client = connect_player(&net, 7793, "cy", &ticket);
    let mut now = 0u64;
    let step = |now: &mut u64,
                steps: u32,
                server: &mut NetServer<TicketGame, LoopbackSocket>,
                client: &mut NetClient<TicketGame, LoopbackSocket>| {
        for _ in 0..steps {
            *now += STEP_MS;
            client.update(*now);
            server.receive(*now);
            server.flush(*now);
        }
    };
    step(&mut now, 100, &mut server, &mut client);
    assert!(client.is_connected(), "pending admit, state {:?}", client.state());
    handle.refuse(&ticket);
    step(&mut now, 20, &mut server, &mut client);
    assert!(
        server.peer_rows().is_empty(),
        "a late refusal kicks the peer on a receive + flush loop, rows {:?}",
        server.peer_rows()
    );
    match client.state() {
        ClientState::Ended(end) => assert_eq!(end.as_str(), "kicked", "a late refusal kicks, got {end:?}"),
        other => panic!("expected a kick, got {other:?}"),
    }
}

/// Open store sessions shared with the test.
struct CountedSessions {
    open: Arc<Mutex<HashSet<Vec<u8>>>>,
}

impl JoinTicketCheck for CountedSessions {
    fn check_ticket(&self, ticket: &[u8]) -> Result<(), TicketRefused> {
        let mut open = self
            .open
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !open.insert(ticket.to_vec()) {
            return Err(TicketRefused);
        }
        Ok(())
    }

    fn end_ticket(&self, ticket: &[u8]) {
        self.open
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(ticket);
    }
}

/// A player that times out on a server with a resume grace is held dormant (spec 120 § 10.3). Its
/// store session stays open for the resume and ends when the hold ends.
#[test]
fn a_dormant_players_store_session_lasts_until_the_hold_ends() {
    let net = LoopbackNet::new();
    let mut server = NetServer::<TicketGame, _>::with_secret(
        net.bind(addr(7794)).expect("bind server"),
        ServerConfig {
            resume_grace_ms: 2_000,
            ..ServerConfig::default()
        },
        [16; 32],
    );
    let open = Arc::new(Mutex::new(HashSet::new()));
    server.set_ticket_check(CountedSessions {
        open: Arc::clone(&open),
    });
    let ticket = b"ticket-dormant".to_vec();
    let mut client = connect_player(&net, 7794, "dee", &ticket);
    let mut now = 0u64;
    drive(&mut now, 100, &mut server, &mut [&mut client]);
    assert!(client.is_connected(), "admitted, state {:?}", client.state());
    let is_open = |open: &Arc<Mutex<HashSet<Vec<u8>>>>| {
        open.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(&ticket)
    };
    // The client goes silent; only the server runs.
    let mut went_dormant = false;
    for _ in 0..60_000 {
        drive(&mut now, 1, &mut server, &mut []);
        if server.dormant_peers().count() == 1 {
            went_dormant = true;
            break;
        }
    }
    assert!(went_dormant, "the silent player went dormant");
    assert!(
        is_open(&open),
        "the dormant player's store session stays open for the resume"
    );
    let mut released = false;
    for _ in 0..1_000 {
        drive(&mut now, 1, &mut server, &mut []);
        if server.dormant_peers().count() == 0 {
            released = true;
            break;
        }
    }
    assert!(released, "the hold ended after the grace");
    assert!(!is_open(&open), "the hold's end ends the store session");
}

/// Open store-session tickets shared with a test.
type OpenTickets = Arc<Mutex<HashSet<Vec<u8>>>>;

/// The server, its open sessions, and the resume ticket of the dormant player's accept.
type DormantFixture = (
    NetServer<TicketGame, LoopbackSocket>,
    OpenTickets,
    Option<[u8; regia_net::handshake::RESUME_TICKET_BYTES]>,
);

/// A server with a resume grace whose hook keeps one store session per ticket, and a player that
/// joined with `ticket` and then went silent until the server held it dormant.
fn dormant_player(net: &LoopbackNet, port: u16, ticket: &[u8], now: &mut u64) -> DormantFixture {
    let mut server = NetServer::<TicketGame, _>::with_secret(
        net.bind(addr(port)).expect("bind server"),
        ServerConfig {
            resume_grace_ms: 30_000,
            ..ServerConfig::default()
        },
        [17; 32],
    );
    let open = Arc::new(Mutex::new(HashSet::new()));
    server.set_ticket_check(CountedSessions {
        open: Arc::clone(&open),
    });
    let mut client = connect_player(net, port, "eve", ticket);
    drive(now, 100, &mut server, &mut [&mut client]);
    assert!(client.is_connected(), "admitted, state {:?}", client.state());
    let resume = client.resume_ticket();
    for _ in 0..60_000 {
        drive(now, 1, &mut server, &mut []);
        if server.dormant_peers().count() == 1 {
            break;
        }
    }
    assert_eq!(
        server.dormant_peers().count(),
        1,
        "the silent player went dormant"
    );
    (server, open, resume)
}

/// A dormant player resumes with its resume token and the same store ticket. The store session it
/// kept carries over; checking the same bytes again would be the store's duplicate refusal.
#[test]
fn a_resume_with_the_held_ticket_keeps_its_store_session() {
    let net = LoopbackNet::new();
    let mut now = 0u64;
    let ticket = b"ticket-resume".to_vec();
    let (mut server, open, resume) = dormant_player(&net, 7795, &ticket, &mut now);
    assert!(resume.is_some(), "the accept carried a resume ticket");
    let mut back = NetClient::<TicketGame, _>::connect(
        net.bind(addr(0)).expect("bind back"),
        addr(7795),
        ClientConfig {
            name: "eve".into(),
            resume,
            ..ClientConfig::default()
        },
        &ticket,
    )
    .expect("join fits");
    drive(&mut now, 100, &mut server, &mut [&mut back]);
    assert!(
        back.is_connected(),
        "the resume is admitted, state {:?}",
        back.state()
    );
    assert_eq!(server.dormant_peers().count(), 0, "the hold was resumed");
    let mut resumed = false;
    while let Some(event) = server.poll_event() {
        resumed |= matches!(event, ServerEvent::PeerResumed { .. });
    }
    assert!(resumed, "the join came back as the dormant peer");
    assert!(
        open.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(&ticket),
        "the store session is still open for the resumed peer"
    );
}

/// A dormant player's process restarted: it joins with the same store ticket and no resume token.
/// The hold and its store session end, and the join is admitted as a new peer.
#[test]
fn a_return_without_the_resume_token_releases_the_dormant_hold() {
    let net = LoopbackNet::new();
    let mut now = 0u64;
    let ticket = b"ticket-restart".to_vec();
    let (mut server, open, _) = dormant_player(&net, 7796, &ticket, &mut now);
    let mut back = connect_player(&net, 7796, "eve", &ticket);
    drive(&mut now, 100, &mut server, &mut [&mut back]);
    assert!(
        back.is_connected(),
        "a token-less return with the same ticket is admitted, state {:?}",
        back.state()
    );
    assert_eq!(server.dormant_peers().count(), 0, "the old hold ended");
    assert_eq!(server.peer_rows().len(), 1);
    assert!(
        open.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(&ticket),
        "the new peer holds the store session"
    );
}
