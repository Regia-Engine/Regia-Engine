// SPDX-License-Identifier: MPL-2.0
//! Network hardening, wave 2 (ADR-3283): the threat-model findings 22, 23 and 28.
//!
//! - 23: the per-IP handshake budget is charged only after the cookie proved the source address,
//!   so forged `ConnectRequest`s in a victim's name cannot keep it out.
//! - 28: an IPv6 /64 is one address for the per-address slot cap and for bans, everywhere; an
//!   IPv4-mapped IPv6 source is its IPv4 address.
//! - 22: `NetSession::connect` trusts a server key on first use, refuses a changed key with
//!   `net_host_key_changed` before the `Hello` (the password) leaves the client, and skips the
//!   store only when the caller says `allow_unpinned`.
//!
//! Everything runs on the in-memory `LoopbackNet` (no OS socket is bound) and a manual clock: no
//! sleeps, no wall-clock asserts.

mod common;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::{Duel, Team};
use regia_net::addr_block::{AddrBlock, same_block};
use regia_net::crypto::{CRYPTO_VERSION, CryptoMode, ServerIdentity, ServerKey};
use regia_net::handshake::ConnectRequest;
use regia_net::known_hosts::{KNOWN_HOSTS_FILE, KnownHosts};
use regia_net::prelude::*;
use regia_net::rate_limit::SourceLimiter;
use regia_net::session::NetSession;
use regia_net::session::bans::address_matches;
use regia_net::session::lifecycle::{
    AuthRequestId, AuthResult, ClientSessionState, HostOptions, PeerAddr, SessionAuthenticator,
    SessionSettings,
};
use regia_net::session::manifest::ContentManifest;
use regia_net::session::net_session::{ConnectOptions, SessionError};
use regia_net::session::proto::{DisconnectReason as SessionEnd, Hello};
use regia_net::socket::DatagramSocket;
use regia_net::{LoopbackSocket, protocol_id};

const STEP_MS: u64 = 10;
const SERVER_PORT: u16 = 7777;

fn server_addr() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], SERVER_PORT))
}

fn client_on(net: &LoopbackNet, at: SocketAddr, to: SocketAddr) -> NetClient<Duel, LoopbackSocket> {
    NetClient::connect(
        net.bind(at).expect("bind client"),
        to,
        ClientConfig {
            name: "victim".into(),
            ..ClientConfig::default()
        },
        &Team(0),
    )
    .expect("join fits")
}

fn connect_request(nonce: u64) -> Vec<u8> {
    ConnectRequest {
        protocol_id: protocol_id::<Duel>(),
        client_nonce: nonce,
        crypto_version: CRYPTO_VERSION,
        crypto_required: false,
    }
    .to_datagram()
}

// ---- Finding 23: the handshake budget is charged after the cookie --------------------------------

/// Finding 23 (ported from the threat-model lane, un-ignored): the per-IP handshake budget is
/// spent before the source address is proved, so anyone forging a victim's address at the
/// budget's rate keeps that victim from ever getting a cookie.
#[test]
fn finding_23_forged_requests_do_not_keep_a_new_client_from_joining() {
    let net = LoopbackNet::new();
    let mut server: NetServer<Duel, LoopbackSocket> = NetServer::with_secret(
        net.bind(server_addr()).expect("bind server"),
        ServerConfig {
            handshakes_per_ip_per_s: 4,
            ..ServerConfig::default()
        },
        [7; 32],
    );
    // The forger shares the victim's IP in the source field: only the port differs here.
    let victim_ip = [127, 0, 0, 2];
    let mut forger = net
        .bind(SocketAddr::from((victim_ip, 6000)))
        .expect("bind forger");
    let request = connect_request(99);
    // The flood comes first, so the source's budget is already spent when the victim asks.
    let flood_steps = 100_u64;
    for step in 0..flood_steps {
        for _ in 0..4 {
            forger.send_to(&request, server_addr()).expect("send");
        }
        server.update(step * STEP_MS);
    }
    let mut client = client_on(&net, SocketAddr::from((victim_ip, 5000)), server_addr());
    for step in flood_steps..flood_steps + 500 {
        let now = step * STEP_MS;
        for _ in 0..4 {
            forger.send_to(&request, server_addr()).expect("send");
        }
        client.update(now);
        server.update(now);
        if client.is_connected() {
            return;
        }
    }
    panic!("five seconds of forged requests kept the client out");
}

/// Grouping by /64 does not make forging cheaper: requests from 64 spoofed addresses of the
/// victim's /64, on eight ports each, every step, cost the victim nothing, because no request is
/// charged before its cookie came back.
#[test]
fn forged_requests_from_every_port_of_the_victims_block_cost_it_nothing() {
    let net = LoopbackNet::new();
    let listen: SocketAddr = "[2001:db8:5::1]:7777".parse().expect("addr");
    let mut server: NetServer<Duel, LoopbackSocket> = NetServer::with_secret(
        net.bind(listen).expect("bind server"),
        ServerConfig {
            handshakes_per_ip_per_s: 1,
            ..ServerConfig::default()
        },
        [3; 32],
    );
    let mut client = client_on(&net, "[2001:db8:7::2]:5000".parse().expect("addr"), listen);
    let request = connect_request(5);
    for step in 0..400_u64 {
        let now = step * STEP_MS;
        for port in 0..8_u16 {
            let spoofed: SocketAddr = SocketAddr::new(
                format!("2001:db8:7::{:x}", 0x100 + step as u16 % 64)
                    .parse()
                    .expect("ip"),
                1000 + port,
            );
            // `LoopbackNet` stamps the sender's own address, so the forged source is a socket
            // bound there and dropped at once: the challenge goes nowhere, as for a spoofed source.
            let mut spoof = net.bind(spoofed).expect("bind spoofed source");
            spoof.send_to(&request, listen).expect("send");
        }
        client.update(now);
        server.update(now);
        if client.is_connected() {
            return;
        }
    }
    panic!("forged requests from the victim's /64 kept it out");
}

/// The budget still binds a source that proved its address: a flood of proved join attempts from
/// one address gets the burst (twice the rate) answered and the rest dropped unanswered.
#[test]
fn a_proved_source_is_still_held_to_its_handshake_budget() {
    let net = LoopbackNet::new();
    let mut server: NetServer<Duel, LoopbackSocket> = NetServer::with_secret(
        net.bind(server_addr()).expect("bind server"),
        ServerConfig {
            handshakes_per_ip_per_s: 4,
            password: Some("right".into()),
            crypto: CryptoMode::Off,
            ..ServerConfig::default()
        },
        [11; 32],
    );
    // Forty clients on one address in turn: each completes its own cookie round trip and then
    // offers a wrong password. Only the per-IP budget decides how many get an answer.
    let mut refused = 0;
    let mut answered = 0;
    for i in 0..40_u64 {
        let mut client: NetClient<Duel, LoopbackSocket> = NetClient::connect(
            net.bind(SocketAddr::from(([127, 0, 0, 3], 5000 + i as u16)))
                .expect("bind"),
            server_addr(),
            ClientConfig {
                password: Some("wrong".into()),
                crypto: CryptoMode::Off,
                ..ClientConfig::default()
            },
            &Team(0),
        )
        .expect("join fits");
        // All forty fall in the same second of the server's clock.
        for _ in 0..6 {
            client.update(i);
            server.update(i);
        }
        while let Some(event) = client.poll_event() {
            if let ClientEvent::Ended(_) = event {
                answered += 1;
            }
        }
        refused = server.handshakes_refused();
    }
    assert_eq!(answered, 8, "the burst only");
    assert!(refused >= 32, "the rest were dropped by the budget ({refused})");
}

// ---- Finding 28: an IPv6 /64 is one address ------------------------------------------------------

fn v6_server(net: &LoopbackNet, config: ServerConfig) -> (NetServer<Duel, LoopbackSocket>, SocketAddr) {
    let listen: SocketAddr = "[2001:db8::ff]:7777".parse().expect("addr");
    let server = NetServer::with_secret(net.bind(listen).expect("bind server"), config, [7; 32]);
    (server, listen)
}

fn run(
    server: &mut NetServer<Duel, LoopbackSocket>,
    clients: &mut [NetClient<Duel, LoopbackSocket>],
    steps: std::ops::Range<u64>,
) {
    for step in steps {
        let now = step * STEP_MS;
        for client in clients.iter_mut() {
            client.update(now);
        }
        server.update(now);
    }
}

/// Finding 28 (ported, un-ignored): only the handshake table grouped IPv6 sources by /64. The
/// per-address slot cap counted exact addresses, so one /64 rotated addresses and took every slot.
#[test]
fn finding_28_one_ipv6_block_is_one_address_for_the_slot_cap() {
    let net = LoopbackNet::new();
    let (mut server, listen) = v6_server(
        &net,
        ServerConfig {
            max_peers_per_ip: 1,
            ..ServerConfig::default()
        },
    );
    let mut clients: Vec<NetClient<Duel, LoopbackSocket>> = ["[2001:db8::1]:5000", "[2001:db8::2]:5000"]
        .iter()
        .map(|at| client_on(&net, at.parse().expect("addr"), listen))
        .collect();
    run(&mut server, &mut clients, 0..200);
    assert_eq!(server.session().len(), 1, "one /64 took more than one slot");
}

/// Sixteen addresses of one /64 against a cap of four: four slots, and a client from the next /64
/// still gets in.
#[test]
fn one_block_cannot_exhaust_the_slots_but_its_neighbor_still_joins() {
    let net = LoopbackNet::new();
    let (mut server, listen) = v6_server(
        &net,
        ServerConfig {
            max_peers: 16,
            max_peers_per_ip: 4,
            ..ServerConfig::default()
        },
    );
    let mut clients: Vec<NetClient<Duel, LoopbackSocket>> = (1..=16_u16)
        .map(|n| {
            client_on(
                &net,
                format!("[2001:db8:0:0:{n:x}::1]:5000").parse().expect("addr"),
                listen,
            )
        })
        .collect();
    run(&mut server, &mut clients, 0..300);
    assert_eq!(server.session().len(), 4, "one /64 holds the cap");
    let mut neighbor = vec![client_on(
        &net,
        "[2001:db8:0:1::1]:5000".parse().expect("addr"),
        listen,
    )];
    run(&mut server, &mut neighbor, 300..500);
    assert!(neighbor[0].is_connected(), "the next /64 is another address");
    assert_eq!(server.session().len(), 5);
}

/// A ban on one address of a /64 holds for the whole /64: a new address in it cannot join; one in
/// the next /64 can. Banning the peer by id (`NetServer::ban`) does the same.
#[test]
fn a_ban_holds_for_the_whole_ipv6_block() {
    let net = LoopbackNet::new();
    let (mut server, listen) = v6_server(&net, ServerConfig::default());
    let mut first = vec![client_on(
        &net,
        "[2001:db8::1]:5000".parse().expect("addr"),
        listen,
    )];
    run(&mut server, &mut first, 0..100);
    assert!(first[0].is_connected());
    let peer = server.session().peers().next().map(|p| p.id).expect("a peer");
    assert!(server.ban(peer, "cheating", 0));

    let mut evader = vec![client_on(
        &net,
        "[2001:db8::abcd:1]:5000".parse().expect("addr"),
        listen,
    )];
    run(&mut server, &mut evader, 100..400);
    assert!(
        !evader[0].is_connected(),
        "a new address in the banned /64 got in"
    );
    assert_eq!(server.session().len(), 0);

    let mut neighbor = vec![client_on(
        &net,
        "[2001:db8:0:1::1]:5000".parse().expect("addr"),
        listen,
    )];
    run(&mut server, &mut neighbor, 400..600);
    assert!(neighbor[0].is_connected(), "the next /64 is not banned");

    // `ban_ip` with any address of the block removes and refuses the block; unban lifts it.
    server.ban_ip("2001:db8:0:1::9".parse().expect("ip"), "spam", 0);
    assert_eq!(
        server.session().len(),
        0,
        "the neighbor's block was banned through another address"
    );
    assert!(server.unban_ip("2001:db8:0:1::77".parse().expect("ip")));
    assert!(
        !server.unban_ip("2001:db8:0:1::77".parse().expect("ip")),
        "one entry per block"
    );
}

/// An IPv4-mapped IPv6 source is its IPv4 address: the per-address cap counts both spellings as
/// one, and a ban on the IPv4 address refuses the mapped one.
#[test]
fn an_ipv4_mapped_source_is_its_ipv4_address() {
    let net = LoopbackNet::new();
    let mut server: NetServer<Duel, LoopbackSocket> = NetServer::with_secret(
        net.bind(server_addr()).expect("bind server"),
        ServerConfig {
            max_peers_per_ip: 1,
            ..ServerConfig::default()
        },
        [5; 32],
    );
    let mut clients = vec![
        client_on(&net, "127.0.0.4:5000".parse().expect("addr"), server_addr()),
        client_on(
            &net,
            "[::ffff:127.0.0.4]:5001".parse().expect("addr"),
            server_addr(),
        ),
    ];
    run(&mut server, &mut clients, 0..200);
    assert_eq!(
        server.session().len(),
        1,
        "two spellings of one address took two slots"
    );

    server.ban_ip("127.0.0.4".parse().expect("ip"), "spam", 0);
    let mut mapped = vec![client_on(
        &net,
        "[::ffff:127.0.0.4]:5002".parse().expect("addr"),
        server_addr(),
    )];
    run(&mut server, &mut mapped, 200..400);
    assert!(
        !mapped[0].is_connected(),
        "the mapped spelling evaded the IPv4 ban"
    );
}

/// The shared helper: one IPv4 address, one IPv6 /64, mapped addresses normalized.
#[test]
fn the_address_block_helper() {
    let ip = |s: &str| s.parse::<std::net::IpAddr>().expect("ip");
    assert!(same_block(
        ip("2001:db8:1:2::1"),
        ip("2001:db8:1:2:ffff:ffff:ffff:ffff")
    ));
    assert!(!same_block(ip("2001:db8:1:2::1"), ip("2001:db8:1:3::1")));
    assert!(same_block(ip("::ffff:10.0.0.1"), ip("10.0.0.1")));
    assert!(!same_block(ip("10.0.0.1"), ip("10.0.0.2")));
    assert_eq!(
        AddrBlock::of(ip("2001:db8:1:2:3:4:5:6")).network(),
        ip("2001:db8:1:2::")
    );
    assert_eq!(AddrBlock::of(ip("::ffff:192.0.2.9")).network(), ip("192.0.2.9"));
    assert_eq!(
        AddrBlock::of(ip("2001:db8:1:2::5")).to_string(),
        "2001:db8:1:2::/64"
    );
    assert_eq!(AddrBlock::of(ip("192.0.2.9")).to_string(), "192.0.2.9");
}

/// The pre-session token buckets key on the block too: a /64 that rotates addresses shares one
/// bucket.
#[test]
fn the_source_limiter_keys_on_the_block() {
    let mut limiter = SourceLimiter::new(1, 65_536, [4; 32]);
    let ip = |n: u16| std::net::IpAddr::from([0x2001, 0xdb8, 0, 0, n, 0, 0, 1]);
    assert!(limiter.admit(ip(1), 0));
    assert!(limiter.admit(ip(2), 0), "the burst is two");
    assert!(
        !limiter.admit(ip(3), 0),
        "a third address of the /64 shares the bucket"
    );
    assert!(limiter.admit(std::net::IpAddr::from([0x2001, 0xdb8, 0, 1, 0, 0, 0, 1]), 0));
}

/// Session-layer ban entries (`bans.ron`): an exact IPv6 address bans its /64; an explicit
/// `/128` stays one address; IPv4 entries are unchanged.
#[test]
fn a_ban_list_address_entry_covers_its_ipv6_block() {
    let ip = |s: &str| s.parse::<std::net::IpAddr>().expect("ip");
    assert!(address_matches("2001:db8::1", ip("2001:db8::2")));
    assert!(address_matches("2001:db8::1", ip("2001:db8::ffff:1")));
    assert!(!address_matches("2001:db8::1", ip("2001:db8:0:1::1")));
    assert!(address_matches("2001:db8::1/128", ip("2001:db8::1")));
    assert!(
        !address_matches("2001:db8::1/128", ip("2001:db8::2")),
        "an explicit prefix is kept"
    );
    assert!(address_matches("10.0.0.1", ip("::ffff:10.0.0.1")));
    assert!(!address_matches("10.0.0.1", ip("10.0.0.2")));
    assert!(address_matches("::ffff:10.0.0.1", ip("10.0.0.1")));
    assert!(!address_matches("::ffff:10.0.0.1", ip("10.0.0.2")));
}

// ---- Finding 22: session.connect pins the server key ---------------------------------------------

/// A store path in a fresh directory of its own (pid plus a counter), never created yet.
fn temp_store(tag: &str) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("regia-net-hardening-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir.join(KNOWN_HOSTS_FILE)
}

/// Counts the `Hello`s the server saw and keeps the passwords they carried.
#[derive(Clone, Default)]
struct HelloSpy {
    hellos: Arc<AtomicUsize>,
    passwords: Arc<std::sync::Mutex<Vec<String>>>,
}

impl SessionAuthenticator for HelloSpy {
    fn check(&mut self, _: AuthRequestId, hello: &Hello, _: &PeerAddr) -> AuthResult {
        self.hellos.fetch_add(1, Ordering::SeqCst);
        if let Some(pw) = &hello.password
            && let Ok(mut list) = self.passwords.lock()
        {
            list.push(pw.clone());
        }
        AuthResult::Accept {
            platform_id: None,
            reserved: false,
        }
    }
}

const SECRET: &str = "hunter2 the password";

/// A sealed server with `identity` at `at` on its own network, with a spy on its `Hello`s.
fn sealed_host(net: &LoopbackNet, at: SocketAddr, identity: ServerIdentity) -> (NetSession, HelloSpy) {
    let settings = SessionSettings {
        encryption: CryptoMode::Required,
        ..SessionSettings::default()
    };
    let mut server = NetSession::new(settings, ContentManifest::build("game", "1", 1, 0, &[]));
    server.set_manual_clock(0);
    server.set_identity(Some(identity));
    server
        .host_on(
            net.bind(at).expect("bind"),
            HostOptions {
                password: SECRET.into(),
                level: "levels/a.level.ron".into(),
                ..HostOptions::default()
            },
            true,
        )
        .expect("host");
    let spy = HelloSpy::default();
    server
        .server_mut()
        .expect("server")
        .set_authenticator(Box::new(spy.clone()));
    (server, spy)
}

/// A sealed listen host with no explicit identity: it loads or creates the persistent identity
/// at `identity_path` instead (ADR-4936).
fn listen_host(net: &LoopbackNet, at: SocketAddr, identity_path: &Path) -> (NetSession, HelloSpy) {
    let settings = SessionSettings {
        encryption: CryptoMode::Required,
        ..SessionSettings::default()
    };
    let mut server = NetSession::new(settings, ContentManifest::build("game", "1", 1, 0, &[]));
    server.set_manual_clock(0);
    server.set_listen_identity_path(Some(identity_path.to_path_buf()));
    server
        .host_on(
            net.bind(at).expect("bind"),
            HostOptions {
                password: SECRET.into(),
                level: "levels/a.level.ron".into(),
                ..HostOptions::default()
            },
            true,
        )
        .expect("host");
    let spy = HelloSpy::default();
    server
        .server_mut()
        .expect("server")
        .set_authenticator(Box::new(spy.clone()));
    (server, spy)
}

fn sealed_client(store: Option<&Path>) -> NetSession {
    let settings = SessionSettings {
        encryption: CryptoMode::Required,
        ..SessionSettings::default()
    };
    let mut client = NetSession::new(settings, ContentManifest::build("game", "1", 1, 0, &[]));
    client.set_manual_clock(0);
    client.set_known_hosts(store.map(Path::to_path_buf));
    client
}

/// Connects from an ephemeral port: on loopback for a loopback server, else from a remote address.
fn connect(
    client: &mut NetSession,
    net: &LoopbackNet,
    at: SocketAddr,
    opts: ConnectOptions,
) -> Result<(), SessionError> {
    let local = if at.ip().is_loopback() {
        SocketAddr::from(([127, 0, 0, 2], 0))
    } else {
        SocketAddr::from(([198, 51, 100, 9], 0))
    };
    client.connect_on(net.bind(local).expect("bind"), at, opts)
}

fn with_password(allow_unpinned: bool) -> ConnectOptions {
    ConnectOptions {
        password: Some(SECRET.into()),
        allow_unpinned,
        ..ConnectOptions::default()
    }
}

/// Steps both until the client is in game or disconnected (at most `steps` steps).
fn drive(server: &mut NetSession, client: &mut NetSession, steps: usize) -> ClientSessionState {
    for _ in 0..steps {
        client.advance_manual_clock(STEP_MS);
        server.advance_manual_clock(STEP_MS);
        client.update();
        server.update();
        let state = client.state();
        if matches!(
            state,
            ClientSessionState::InGame | ClientSessionState::Disconnected(Some(_))
        ) {
            return state;
        }
    }
    client.state()
}

fn host_addr() -> SocketAddr {
    SocketAddr::from(([192, 0, 2, 7], SERVER_PORT))
}

/// Finding 22: the first connection records the key; a server at the same address with another
/// key (an attacker in the middle) is refused with `net_host_key_changed` before the `Hello` with
/// the password leaves the client, and the store keeps the first key.
#[test]
fn finding_22_a_substituted_server_key_is_refused_before_the_password_is_sent() {
    let store = temp_store("substituted");
    let real = ServerIdentity::generate().expect("key");
    let real_key = real.public();

    // First use: the real server.
    let net = LoopbackNet::new();
    let (mut server, spy) = sealed_host(&net, host_addr(), real);
    let mut client = sealed_client(Some(&store));
    connect(&mut client, &net, host_addr(), with_password(false)).expect("connect");
    assert_eq!(drive(&mut server, &mut client, 1000), ClientSessionState::InGame);
    assert_eq!(
        spy.hellos.load(Ordering::SeqCst),
        1,
        "control: the real server got the Hello"
    );
    let pinned = KnownHosts::load(&store).expect("store");
    assert_eq!(
        pinned.get(&host_addr().to_string()).map(|h| h.key),
        Some(real_key)
    );

    // Later: someone else answers at that address with their own key.
    let net = LoopbackNet::new();
    let attacker = ServerIdentity::generate().expect("key");
    let (mut mitm, mitm_spy) = sealed_host(&net, host_addr(), attacker);
    let mut client = sealed_client(Some(&store));
    connect(&mut client, &net, host_addr(), with_password(false)).expect("connect");
    let state = drive(&mut mitm, &mut client, 1000);
    assert_eq!(
        state,
        ClientSessionState::Disconnected(Some(SessionEnd::TransportError("net_host_key_changed".into())))
    );
    assert_eq!(
        mitm_spy.hellos.load(Ordering::SeqCst),
        0,
        "the Hello reached the attacker"
    );
    assert!(mitm_spy.passwords.lock().expect("lock").is_empty());
    let after = KnownHosts::load(&store).expect("store");
    assert_eq!(
        after.get(&host_addr().to_string()).map(|h| h.key),
        Some(real_key),
        "the pin was kept"
    );
}

/// A store that already pins another key refuses on the first visit of this process, too (the
/// key reaches the transport as a pin, so the handshake itself fails).
#[test]
fn a_key_pinned_by_an_earlier_run_is_enforced() {
    let store = temp_store("earlier");
    let mut hosts = KnownHosts::new();
    let other: ServerKey = ServerIdentity::generate().expect("key").public();
    hosts.trust(&host_addr().to_string(), other).expect("trust");
    hosts.save(&store).expect("save");

    let net = LoopbackNet::new();
    let (mut server, spy) = sealed_host(&net, host_addr(), ServerIdentity::generate().expect("key"));
    let mut client = sealed_client(Some(&store));
    connect(&mut client, &net, host_addr(), with_password(false)).expect("connect");
    let state = drive(&mut server, &mut client, 1000);
    assert_eq!(
        state,
        ClientSessionState::Disconnected(Some(SessionEnd::TransportError("net_host_key_changed".into())))
    );
    assert_eq!(spy.hellos.load(Ordering::SeqCst), 0);
}

/// `allow_unpinned` (LAN development) neither reads nor writes the store.
#[test]
fn allow_unpinned_skips_the_store() {
    let store = temp_store("unpinned");
    let mut hosts = KnownHosts::new();
    let other: ServerKey = ServerIdentity::generate().expect("key").public();
    hosts.trust(&host_addr().to_string(), other).expect("trust");
    hosts.save(&store).expect("save");
    let before = std::fs::read(&store).expect("read");

    let net = LoopbackNet::new();
    let (mut server, spy) = sealed_host(&net, host_addr(), ServerIdentity::generate().expect("key"));
    let mut client = sealed_client(Some(&store));
    connect(&mut client, &net, host_addr(), with_password(true)).expect("connect");
    assert_eq!(drive(&mut server, &mut client, 1000), ClientSessionState::InGame);
    assert_eq!(spy.hellos.load(Ordering::SeqCst), 1);
    assert_eq!(
        std::fs::read(&store).expect("read"),
        before,
        "the store was not touched"
    );
}

/// An explicit `server_key` wins over the store, which is neither read nor written.
#[test]
fn an_explicit_server_key_wins_over_the_store() {
    let store = temp_store("explicit");
    let real = ServerIdentity::generate().expect("key");
    let real_key = real.public();
    let net = LoopbackNet::new();
    let (mut server, _spy) = sealed_host(&net, host_addr(), real);
    let mut client = sealed_client(Some(&store));
    let opts = ConnectOptions {
        server_key: Some(real_key),
        ..with_password(false)
    };
    connect(&mut client, &net, host_addr(), opts).expect("connect");
    assert_eq!(drive(&mut server, &mut client, 1000), ClientSessionState::InGame);
    assert!(!store.exists(), "an explicit pin writes nothing");
}

/// A damaged store refuses the connection at once (`net_known_hosts_unreadable`) and is never
/// overwritten; `allow_unpinned` still connects.
#[test]
fn a_damaged_store_refuses_and_is_kept() {
    let store = temp_store("damaged");
    if let Some(dir) = store.parent() {
        std::fs::create_dir_all(dir).expect("dir");
    }
    std::fs::write(&store, "not ron at all").expect("write");
    let net = LoopbackNet::new();
    let (_server, _spy) = sealed_host(&net, host_addr(), ServerIdentity::generate().expect("key"));
    let mut client = sealed_client(Some(&store));
    let err = connect(&mut client, &net, host_addr(), with_password(false)).expect_err("refused");
    assert_eq!(err.code(), "net_known_hosts_unreadable");
    assert_eq!(client.role_name(), "idle");
    assert_eq!(std::fs::read_to_string(&store).expect("read"), "not ron at all");
    connect(&mut client, &net, host_addr(), with_password(true)).expect("allow_unpinned connects");
}

/// Merge review (ADR-4936): a listen host without an explicit identity used to derive a fresh
/// per-run key, so every returning client whose known-hosts store pinned the previous key ended
/// with `net_host_key_changed` on the next evening. With a listen identity path (the plugin sets
/// `user://listen-server.key` when the app has a `user://` mount) the key is created on the first
/// host and reused by later runs: the returning client connects.
#[test]
fn a_listen_host_reuses_its_persisted_identity() {
    let dir = temp_store("listen-id").parent().expect("parent").to_path_buf();
    let path = dir.join("listen-server.key");
    let store = dir.join(KNOWN_HOSTS_FILE);

    // First evening: hosting creates the identity; the client pins it on first use.
    let net = LoopbackNet::new();
    let (mut host, spy) = listen_host(&net, host_addr(), &path);
    let key = host.server_key().expect("a hosted sealed server has a key");
    assert!(path.is_file(), "the identity file is created on the first host");
    let mut client = sealed_client(Some(&store));
    connect(&mut client, &net, host_addr(), with_password(false)).expect("connect");
    assert_eq!(drive(&mut host, &mut client, 1000), ClientSessionState::InGame);
    assert_eq!(spy.hellos.load(Ordering::SeqCst), 1);
    let pinned = KnownHosts::load(&store).expect("store");
    assert_eq!(pinned.get(&host_addr().to_string()).map(|h| h.key), Some(key));
    drop(client);
    drop(host);

    // Second evening: a new process hosts on the same path. The key is the same, so the
    // returning client connects instead of ending with `net_host_key_changed`.
    let net = LoopbackNet::new();
    let (mut host, spy) = listen_host(&net, host_addr(), &path);
    assert_eq!(host.server_key(), Some(key), "the persisted identity is reused");
    let mut client = sealed_client(Some(&store));
    connect(&mut client, &net, host_addr(), with_password(false)).expect("connect");
    assert_eq!(drive(&mut host, &mut client, 1000), ClientSessionState::InGame);
    assert_eq!(
        spy.hellos.load(Ordering::SeqCst),
        1,
        "the Hello reached the same-key host"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Without a listen identity path (a bare `NetSession`, the default when nothing set one), a
/// listen host keeps the ephemeral per-run key: two runs differ, and the client's pin is what
/// refuses the second run with `net_host_key_changed`.
#[test]
fn a_listen_host_without_a_path_stays_ephemeral() {
    let store = temp_store("listen-ephemeral");
    let net = LoopbackNet::new();
    let (mut host, _spy) = sealed_host(&net, host_addr(), ServerIdentity::generate().expect("key"));
    let first = host.server_key().expect("key");
    // First evening: the client pins this run's key on first use.
    let mut client = sealed_client(Some(&store));
    connect(&mut client, &net, host_addr(), with_password(false)).expect("connect");
    assert_eq!(drive(&mut host, &mut client, 1000), ClientSessionState::InGame);
    drop(client);
    drop(host);

    // A new run without a path makes a new key: the pin from the first evening refuses before
    // the Hello — the pre-ADR-4936 behavior a listen identity path exists to fix.
    let net = LoopbackNet::new();
    let (mut host, spy) = sealed_host(&net, host_addr(), ServerIdentity::generate().expect("key"));
    assert_ne!(host.server_key(), Some(first));
    let mut client = sealed_client(Some(&store));
    connect(&mut client, &net, host_addr(), with_password(false)).expect("connect");
    let state = drive(&mut host, &mut client, 1000);
    assert_eq!(
        state,
        ClientSessionState::Disconnected(Some(SessionEnd::TransportError("net_host_key_changed".into())))
    );
    assert_eq!(spy.hellos.load(Ordering::SeqCst), 0);
}

/// A plain link (loopback, `encryption: auto`) has no key, so nothing is recorded.
#[test]
fn a_plain_link_pins_nothing() {
    let store = temp_store("plain");
    let net = LoopbackNet::new();
    let settings = SessionSettings::default();
    let manifest = ContentManifest::build("game", "1", 1, 0, &[]);
    let mut server = NetSession::new(settings.clone(), manifest.clone());
    server.set_manual_clock(0);
    server
        .host_on(
            net.bind(server_addr()).expect("bind"),
            HostOptions {
                level: "levels/a.level.ron".into(),
                ..HostOptions::default()
            },
            true,
        )
        .expect("host");
    let mut client = NetSession::new(settings, manifest);
    client.set_manual_clock(0);
    client.set_known_hosts(Some(store.clone()));
    connect(&mut client, &net, server_addr(), ConnectOptions::default()).expect("connect");
    assert_eq!(drive(&mut server, &mut client, 1000), ClientSessionState::InGame);
    assert!(!client.link_sealed());
    assert!(!store.exists());
}
