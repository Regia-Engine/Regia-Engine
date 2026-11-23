// SPDX-License-Identifier: MPL-2.0
//! Endpoint-level regressions of the unified transport policy (ADR-7995): one-way outages raise
//! one Interrupted/Restored pair, never one pair per probe (review row 14), on real
//! `NetServer`/`NetClient` endpoints over in-memory sockets with a virtual clock.
use regia_net::handshake::{Challenge, PacketKind, split_datagram};
use regia_net::prelude::*;
use regia_net::socket::LoopbackSocket;
use std::io;
use std::net::SocketAddr;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// A socket whose outbound direction can be cut: datagrams it sends while `muted` vanish.
struct OneWaySocket {
    socket: LoopbackSocket,
    muted: Arc<AtomicBool>,
}

impl DatagramSocket for OneWaySocket {
    fn send_to(&mut self, bytes: &[u8], dest: SocketAddr) -> io::Result<()> {
        if self.muted.load(Ordering::Relaxed) {
            Ok(())
        } else {
            self.socket.send_to(bytes, dest)
        }
    }
    fn recv_from(&mut self, buf: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
        self.socket.recv_from(buf)
    }
    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }
}

fn addr(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

#[derive(Debug)]
struct Game;
impl NetGame for Game {
    type Message = u32;
    type Input = u32;
    type JoinData = ();
    const GAME_ID: &'static str = "regia.test.transport_unify";
    const PROTOCOL_VERSION: u32 = 1;
}

struct Pair {
    server: NetServer<Game, OneWaySocket>,
    client: NetClient<Game, OneWaySocket>,
    /// Cuts server -> client.
    server_muted: Arc<AtomicBool>,
    /// Cuts client -> server.
    client_muted: Arc<AtomicBool>,
    now: u64,
    peer: PeerId,
    server_events: Vec<String>,
    client_events: Vec<String>,
}

impl Pair {
    fn new() -> Self {
        Self::with_tick_sync(ServerConfig::default().tick_sync_interval_ms)
    }

    /// A pair whose server sends its tick sync every `tick_sync_interval_ms`. A long interval
    /// leaves keepalives as the only traffic of an idle link.
    fn with_tick_sync(tick_sync_interval_ms: u64) -> Self {
        let net = LoopbackNet::new();
        let server_muted = Arc::new(AtomicBool::new(false));
        let client_muted = Arc::new(AtomicBool::new(false));
        let server = NetServer::with_secret(
            OneWaySocket {
                socket: net.bind(addr(7777)).expect("bind"),
                muted: server_muted.clone(),
            },
            ServerConfig {
                crypto: regia_net::crypto::CryptoMode::Required,
                tick_sync_interval_ms,
                ..Default::default()
            },
            [5; 32],
        );
        let client = NetClient::connect(
            OneWaySocket {
                socket: net.bind(addr(0)).expect("bind"),
                muted: client_muted.clone(),
            },
            addr(7777),
            ClientConfig {
                crypto: regia_net::crypto::CryptoMode::Required,
                ..Default::default()
            },
            &(),
        )
        .expect("connect");
        let mut p = Self {
            server,
            client,
            server_muted,
            client_muted,
            now: 0,
            peer: PeerId(0),
            server_events: vec![],
            client_events: vec![],
        };
        for _ in 0..100 {
            p.step();
        }
        p.peer = p.client.peer_id().expect("connected");
        // Game traffic in both directions ends the start grace.
        p.client.send(Channel::GAME_RPC, &u32::MAX).expect("send");
        p.server.send(p.peer, Channel::GAME_RPC, &u32::MAX).expect("send");
        for _ in 0..100 {
            p.step();
        }
        p.server_events.clear();
        p.client_events.clear();
        p
    }

    fn step(&mut self) {
        self.now += 10;
        self.client.receive(self.now);
        self.server.receive(self.now);
        self.client.flush(self.now);
        self.server.flush(self.now);
        while let Some(e) = self.client.poll_event() {
            if !matches!(e, ClientEvent::Message { .. }) {
                self.client_events.push(format!("{e:?}"));
            }
        }
        while let Some(e) = self.server.poll_event() {
            if !matches!(e, ServerEvent::Message { .. }) {
                self.server_events.push(format!("{e:?}"));
            }
        }
    }

    /// The same pair with the good-wifi impairment of the soak profiles on both directions.
    fn on_good_wifi(mut self) -> Self {
        let wifi = ConditionerConfig {
            enabled: true,
            delay_ms: 4,
            jitter_ms: 4,
            loss: 0.002,
            reorder: 0.001,
            reorder_max_ms: 10,
            duplicate: 0.0005,
            seed: 0x9E37_79B9_7F4A_7C15,
            ..Default::default()
        };
        self.server.set_conditioner(wifi.clone());
        self.client.set_conditioner(ConditionerConfig {
            seed: 0xC2B2_AE3D_27D4_EB4F,
            ..wifi
        });
        self.run(100);
        self.server_events.clear();
        self.client_events.clear();
        self
    }

    fn run(&mut self, steps: usize) {
        for _ in 0..steps {
            self.step();
        }
    }
}

fn count(events: &[String], name: &str) -> usize {
    events.iter().filter(|e| e.starts_with(name)).count()
}

/// Row 14: the client's datagrams stop reaching the server for ten seconds while the server's
/// still arrive. The server reports one interruption; the client, which hears the server the
/// whole time, reports none (the server must keep its keepalive floor, not back off to probes).
#[test]
fn row14_client_to_server_outage_raises_one_pair_on_the_server_only() {
    // An idle link: no tick sync inside the outage, so only keepalives reach the client.
    for wifi in [false, true] {
        let p = Pair::with_tick_sync(60_000);
        client_to_server_outage(if wifi { p.on_good_wifi() } else { p });
    }
}

fn client_to_server_outage(mut p: Pair) {
    p.client_muted.store(true, Ordering::Relaxed);
    p.run(1_000);
    assert!(p.client.is_connected() && p.server.session().peer(p.peer).is_some());
    println!(
        "ROW14 c2s during: server={:?} client={:?}",
        p.server_events, p.client_events
    );
    assert_eq!(count(&p.server_events, "PeerInterrupted"), 1);
    assert_eq!(count(&p.server_events, "PeerRestored"), 0);
    assert_eq!(
        count(&p.client_events, "PeerInterrupted"),
        0,
        "the client hears the server throughout"
    );
    p.client_muted.store(false, Ordering::Relaxed);
    p.run(300);
    assert_eq!(count(&p.server_events, "PeerInterrupted"), 1);
    assert_eq!(count(&p.server_events, "PeerRestored"), 1);
    assert_eq!(count(&p.client_events, "PeerInterrupted"), 0);
    assert_eq!(count(&p.client_events, "PeerRestored"), 0);
}

/// Row 14, the mirrored case: the server's datagrams stop reaching the client. The client
/// backs its probes off (spec 120 § 10.2); the server must not answer each probe with a
/// Restored/Interrupted pair: a probe that acknowledges nothing new is not a restored link.
#[test]
fn row14_server_to_client_outage_raises_one_pair_per_end() {
    for wifi in [false, true] {
        let p = Pair::new();
        server_to_client_outage(if wifi { p.on_good_wifi() } else { p });
    }
}

fn server_to_client_outage(mut p: Pair) {
    p.server_muted.store(true, Ordering::Relaxed);
    p.run(1_500);
    assert!(p.client.is_connected() && p.server.session().peer(p.peer).is_some());
    println!(
        "ROW14 s2c during: server={:?} client={:?}",
        p.server_events, p.client_events
    );
    assert_eq!(count(&p.client_events, "PeerInterrupted"), 1);
    assert_eq!(count(&p.client_events, "PeerRestored"), 0);
    assert_eq!(
        count(&p.server_events, "PeerRestored"),
        0,
        "a probe from a client that cannot hear the server restores nothing"
    );
    assert!(count(&p.server_events, "PeerInterrupted") <= 1);
    p.server_muted.store(false, Ordering::Relaxed);
    p.run(300);
    assert_eq!(count(&p.client_events, "PeerInterrupted"), 1);
    assert_eq!(count(&p.client_events, "PeerRestored"), 1);
    let interrupted = count(&p.server_events, "PeerInterrupted");
    assert_eq!(count(&p.server_events, "PeerRestored"), interrupted);
    assert!(interrupted <= 1, "server events: {:?}", p.server_events);
    assert!(
        !ron::to_string(&p.server.peer_rows())
            .expect("peers")
            .contains("interrupted:true")
    );
}

/// A two-way outage still raises exactly one pair per end, and data queued across it arrives
/// (the bidirectional restore rule must not strand either end).
#[test]
fn two_way_outage_restores_both_ends_once() {
    let mut p = Pair::new();
    p.server_muted.store(true, Ordering::Relaxed);
    p.client_muted.store(true, Ordering::Relaxed);
    p.run(600);
    assert_eq!(count(&p.client_events, "PeerInterrupted"), 1);
    assert_eq!(count(&p.server_events, "PeerInterrupted"), 1);
    p.server_muted.store(false, Ordering::Relaxed);
    p.client_muted.store(false, Ordering::Relaxed);
    let mut restored_at = None;
    for step in 0..300 {
        p.step();
        if restored_at.is_none()
            && count(&p.client_events, "PeerRestored") == 1
            && count(&p.server_events, "PeerRestored") == 1
        {
            restored_at = Some(step);
        }
    }
    println!("TWOWAY restored after {restored_at:?} steps of 10 ms");
    assert_eq!(count(&p.client_events, "PeerRestored"), 1);
    assert_eq!(count(&p.server_events, "PeerRestored"), 1);
    assert_eq!(count(&p.client_events, "PeerInterrupted"), 1);
    assert_eq!(count(&p.server_events, "PeerInterrupted"), 1);
    assert!(
        restored_at.is_some_and(|s| s <= 15),
        "both ends restore within 150 ms of the path returning: {restored_at:?}"
    );
}

/// A socket that records the cookie of every `Challenge` it sends.
struct CookieLogSocket {
    socket: LoopbackSocket,
    cookies: Arc<std::sync::Mutex<Vec<[u8; 16]>>>,
}

impl DatagramSocket for CookieLogSocket {
    fn send_to(&mut self, bytes: &[u8], dest: SocketAddr) -> io::Result<()> {
        if let Some((PacketKind::Challenge, body)) = split_datagram(bytes)
            && let Ok(challenge) = Challenge::decode(body)
            && let Ok(mut cookies) = self.cookies.lock()
            && !cookies.contains(&challenge.cookie)
        {
            cookies.push(challenge.cookie);
        }
        self.socket.send_to(bytes, dest)
    }
    fn recv_from(&mut self, buf: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
        self.socket.recv_from(buf)
    }
    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }
}

/// One join at 1,500 ms each way with `loss`, started so that the server's 10 s cookie bucket
/// turns over while the client still repeats its first request. Returns whether the client
/// connected, how many key exchanges the server ran and how many different cookies it sent.
fn join_across_a_cookie_bucket(seed: u64, loss: f64) -> (bool, u64, usize) {
    let net = LoopbackNet::new();
    let cookies = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut server = NetServer::<Game, _>::with_secret(
        CookieLogSocket {
            socket: net.bind(addr(7777)).expect("bind"),
            cookies: cookies.clone(),
        },
        ServerConfig {
            crypto: regia_net::crypto::CryptoMode::Required,
            ..Default::default()
        },
        [5; 32],
    );
    let mut client = NetClient::<Game, _>::connect(
        net.bind(addr(0)).expect("bind"),
        addr(7777),
        ClientConfig {
            crypto: regia_net::crypto::CryptoMode::Required,
            ..Default::default()
        },
        &(),
    )
    .expect("connect");
    let extreme = ConditionerConfig {
        enabled: true,
        delay_ms: 1_500,
        loss,
        ..Default::default()
    };
    server.set_conditioner(ConditionerConfig {
        seed,
        ..extreme.clone()
    });
    client.set_conditioner(ConditionerConfig {
        seed: seed.rotate_left(17) ^ 0x5DEE_CE66_D1CE_4E5B,
        ..extreme
    });
    // The requests sent at 8,000 and 8,250 ms reach the server at 9,500 and 9,750 ms (one
    // cookie bucket), the rest from 10,000 ms on (the next bucket): both cookies come back.
    let mut now = 8_000u64;
    while now < 40_000 && matches!(client.state(), ClientState::Connecting) {
        client.receive(now);
        server.receive(now);
        client.flush(now);
        server.flush(now);
        now += 10;
    }
    let seen = cookies.lock().map_or(0, |c| c.len());
    (client.is_connected(), server.totals().handshakes, seen)
}

/// Review row 18: joins at the rtt-extreme profile (1,500 ms each way) when the server's cookie
/// bucket turns over while the client is still repeating its first request. The later challenge
/// carries a different, equally valid cookie; the server has stored the share of the first key
/// exchange. The client must finish on that share instead of restarting with its deadline kept
/// (two more 3 s round trips do not fit the 5 s connect timeout). Without loss every join meets
/// the boundary; with 20 % loss most do.
#[test]
fn row18_joins_at_3000ms_rtt_across_a_cookie_bucket_boundary() {
    let (joined, exchanges, cookies) = join_across_a_cookie_bucket(1, 0.0);
    println!("ROW18 loss=0 joined={joined} server_exchanges={exchanges} cookies={cookies}");
    assert_eq!(cookies, 2, "both cookie buckets answered");
    assert!(joined, "the join completes on a clean 3 s path");
    assert_eq!(exchanges, 1, "on the first key exchange");

    let mut joined = 0;
    let mut crossed = 0;
    let mut exchanges = Vec::new();
    for k in 1..=24u64 {
        // Well-mixed seeds: the conditioner's first draws from a small seed are all near zero.
        let (ok, n, cookies) = join_across_a_cookie_bucket(k.wrapping_mul(0x9E37_79B9_7F4A_7C15), 0.2);
        joined += u32::from(ok);
        crossed += u32::from(cookies >= 2);
        exchanges.push(n);
    }
    println!(
        "ROW18 loss=0.2 joined={joined}/24 crossed_boundary={crossed}/24 server_exchanges={exchanges:?}"
    );
    assert!(crossed >= 12, "most runs see both cookies: {crossed}");
    assert_eq!(
        joined, 24,
        "every join completes: exchanges per join {exchanges:?}"
    );
    assert!(
        exchanges.iter().all(|n| *n == 1),
        "one key exchange per join, no restart: {exchanges:?}"
    );
}
