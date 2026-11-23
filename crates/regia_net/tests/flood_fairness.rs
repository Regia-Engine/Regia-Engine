// SPDX-License-Identifier: MPL-2.0
//! Floods and fairness on the server (spec 120 § 11.2, § 11.3; ADR-8181), in process: a seeded
//! good-wifi conditioner on every honest endpoint, the in-memory network, and a 1 ms step clock.
//! Nothing sleeps and nothing reads the wall clock; every flood datagram is generated and counted
//! by the test.
//!
//! - `flood`: one joined peer sends 100,000 valid sealed datagrams a second next to seven
//!   good-wifi players. Their missed input ticks and RTT stay within their no-flood control and
//!   the flooder is removed with `Flood` within 2 s.
//! - bad/good alternation: a peer alternating undecodable and valid packets is removed with
//!   `MalformedPacket` within 60 s (a good packet no longer cancels a bad one).
//! - soak F3: one undecodable message per packet plus a 60 KiB undecodable reliable message each
//!   second is removed.
//! - soak F4: about 190,000 valid messages a second is removed with `Flood`.
//! - guard: an honest peer at its whole send allowance with a 3x burst for 200 ms stays.

use std::collections::BTreeMap;
use std::net::SocketAddr;

use regia_net::LoopbackSocket;
use regia_net::prelude::*;

/// One clock step, milliseconds.
const STEP_MS: u64 = 1;
/// Clients act every this many steps (100 Hz).
const CLIENT_EVERY_MS: u64 = 10;
/// The flood rate: datagrams per second.
const FLOOD_PER_S: u64 = 100_000;
/// Input lead over the client's estimate of the server tick.
const LEAD_TICKS: u64 = 3;

/// A test game with a message the server cannot decode (a hostile or broken client).
#[derive(Debug)]
struct Game;

#[derive(Debug, Clone, PartialEq)]
enum Msg {
    /// A valid message.
    Ping(u8),
    /// Encodes an unknown tag: undecodable at the server.
    Bad,
    /// Encodes an unknown tag followed by `n` bytes: a large undecodable (reliable) message.
    BadBlob(usize),
}

impl NetCodec for Msg {
    fn encode(&self, w: &mut WireWriter) {
        match self {
            Msg::Ping(v) => {
                w.u8(0);
                w.u8(*v);
            }
            Msg::Bad => w.u8(9),
            Msg::BadBlob(n) => {
                w.u8(9);
                w.raw(&vec![0x5A; *n]);
            }
        }
    }

    fn decode(r: &mut WireReader<'_>) -> Result<Self, WireError> {
        match r.u8()? {
            0 => Ok(Msg::Ping(r.u8()?)),
            _ => Err(WireError::Invalid("msg tag")),
        }
    }
}

/// One step left (-1), none (0) or right (+1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Move(i8);

impl NetCodec for Move {
    fn encode(&self, w: &mut WireWriter) {
        w.u8(self.0 as u8);
    }
    fn decode(r: &mut WireReader<'_>) -> Result<Self, WireError> {
        let v = r.u8()? as i8;
        if !(-1..=1).contains(&v) {
            return Err(WireError::Invalid("move"));
        }
        Ok(Move(v))
    }
}

impl NetGame for Game {
    type Message = Msg;
    type Input = Move;
    type JoinData = ();
    const GAME_ID: &'static str = "regia.test.flood_fairness";
    const PROTOCOL_VERSION: u32 = 1;
}

type Server = NetServer<Game, LoopbackSocket>;
type Client = NetClient<Game, LoopbackSocket>;

fn server_addr() -> SocketAddr {
    SocketAddr::from(([10, 0, 0, 1], 7777))
}

/// Each client on its own address (non-loopback, so `CryptoMode::Auto` seals every link).
fn client_addr(k: u8) -> SocketAddr {
    SocketAddr::from(([10, 0, 1, k + 1], 5000))
}

/// The good-wifi profile of the soak harness, one direction: 4 ms + 0-4 ms jitter, 0.2 % loss,
/// 0.1 % reorder (up to 10 ms), 0.05 % duplication.
fn good_wifi(seed: u64) -> ConditionerConfig {
    ConditionerConfig {
        enabled: true,
        seed,
        loss: 0.002,
        duplicate: 0.0005,
        reorder: 0.001,
        reorder_max_ms: 10,
        delay_ms: 4,
        jitter_ms: 4,
        ..ConditionerConfig::default()
    }
}

/// The role a client plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    /// A good-wifi player sending one input per tick.
    Player,
    /// Sends [`FLOOD_PER_S`] sealed datagrams a second (one valid message each) while flooding.
    Flooder,
    /// Alternates a packet carrying an undecodable message and a valid one.
    Alternator,
    /// Soak F3: one undecodable message per packet, plus a 60 KiB undecodable reliable message
    /// each second.
    MalformedStream,
    /// Soak F4: about 190,000 valid unreliable messages a second.
    MessageFlood,
    /// An honest peer at its whole send allowance (`max_packets_per_poll` per tick), bursting 3x
    /// for 200 ms.
    Bursty,
}

struct Peer {
    client: Client,
    role: Role,
    last_input: Option<u64>,
}

struct Rig {
    net: LoopbackNet,
    server: Server,
    peers: Vec<Peer>,
    clock: TickClock,
    now: u64,
    /// When each peer (by index) left, and why.
    left: BTreeMap<usize, (u64, DisconnectReason)>,
    ids: BTreeMap<PeerId, usize>,
    /// Flood datagrams the test generated.
    flood_sent: u64,
    /// When the hostile behavior starts (None: never).
    hostile_from: Option<u64>,
    /// RTT samples of the players, after the hostile start (or the same time in the control).
    rtt: Vec<f64>,
}

fn rig(roles: &[Role], tick_rate_hz: u32, hostile_from: Option<u64>) -> Rig {
    let net = LoopbackNet::new();
    let config = ServerConfig {
        tick_rate_hz,
        conditioner: good_wifi(1),
        ..ServerConfig::default()
    };
    let server = NetServer::with_secret(net.bind(server_addr()).expect("bind server"), config, [7; 32]);
    let peers = roles
        .iter()
        .enumerate()
        .map(|(k, &role)| {
            let honest = matches!(role, Role::Player | Role::Bursty);
            let mut config = ClientConfig {
                name: format!("p{k}"),
                conditioner: if honest {
                    good_wifi(100 + k as u64)
                } else {
                    ConditionerConfig::default()
                },
                ..ClientConfig::default()
            };
            // A hostile peer does not run the engine's pacer (ADR-8301): its sends leave as fast
            // as it makes them, so the server's inbound policy is what stops it (ADR-8530).
            config.link.rate.enabled = honest;
            let socket = net.bind(client_addr(k as u8)).expect("bind client");
            Peer {
                client: NetClient::connect(socket, server_addr(), config, &()).expect("join fits"),
                role,
                last_input: None,
            }
        })
        .collect();
    Rig {
        net,
        server,
        peers,
        clock: TickClock::new(tick_rate_hz, 4),
        now: 0,
        left: BTreeMap::new(),
        ids: BTreeMap::new(),
        flood_sent: 0,
        hostile_from,
        rtt: Vec::new(),
    }
}

impl Rig {
    fn hostile(&self) -> bool {
        self.hostile_from.is_some_and(|t| self.now >= t)
    }

    /// One 1 ms step.
    fn step(&mut self) {
        self.now += STEP_MS;
        let now = self.now;
        let hostile = self.hostile();
        let tick_ms = 1_000 / u64::from(self.server.config().tick_rate_hz);
        for peer in &mut self.peers {
            if !peer.client.is_connected() {
                // Joining (or removed): only the handshake runs.
                if now.is_multiple_of(CLIENT_EVERY_MS) {
                    peer.client.receive(now);
                    peer.client.flush(now);
                }
                continue;
            }
            match peer.role {
                Role::Flooder if hostile => {
                    // 100 sealed datagrams per millisecond, each one valid message.
                    for _ in 0..FLOOD_PER_S / 1_000 {
                        let _ = peer.client.send(Channel::Unreliable, &Msg::Ping(1));
                        peer.client.flush(now);
                        self.flood_sent += 1;
                    }
                }
                Role::MessageFlood if hostile && now.is_multiple_of(CLIENT_EVERY_MS) => {
                    // 1,900 messages per 10 ms: 190,000 a second, packed into full packets.
                    for _ in 0..1_900 {
                        let _ = peer.client.send(Channel::Unreliable, &Msg::Ping(2));
                    }
                }
                Role::Bursty if now.is_multiple_of(tick_ms) => {
                    // Its whole allowance (64 datagrams a tick), 3x for 200 ms from the hostile
                    // start.
                    let burst = self.hostile_from.is_some_and(|t| (t..t + 200).contains(&now));
                    for _ in 0..(if burst { 3 * 64 } else { 64 }) {
                        let _ = peer.client.send(Channel::Unreliable, &Msg::Ping(3));
                        peer.client.flush(now);
                    }
                }
                _ => {}
            }
            if !now.is_multiple_of(CLIENT_EVERY_MS) {
                continue;
            }
            peer.client.receive(now);
            if !peer.client.is_connected() {
                continue;
            }
            if peer.role == Role::Player {
                let target = peer.client.estimated_server_tick(now) + LEAD_TICKS;
                let from = peer.last_input.map_or(target, |t| t + 1);
                for tick in from..=target {
                    let _ = peer.client.send_input(tick, Move(1));
                }
                if from <= target {
                    peer.last_input = Some(target);
                }
            }
            if hostile {
                match peer.role {
                    Role::Alternator => {
                        let msg = if (now / CLIENT_EVERY_MS).is_multiple_of(2) {
                            Msg::Bad
                        } else {
                            Msg::Ping(0)
                        };
                        let _ = peer.client.send(Channel::Unreliable, &msg);
                    }
                    Role::MalformedStream => {
                        let _ = peer.client.send(Channel::Unreliable, &Msg::Bad);
                        if now.is_multiple_of(1_000) {
                            let _ = peer.client.send(Channel::GAME_RPC, &Msg::BadBlob(60 * 1024));
                        }
                    }
                    _ => {}
                }
            }
            peer.client.flush(now);
            while peer.client.poll_event().is_some() {}
        }
        if now.is_multiple_of(tick_ms) {
            self.server_tick();
        }
    }

    /// The server's once-a-tick work: receive, events, tick, flush.
    fn server_tick(&mut self) {
        let now = self.now;
        self.server.receive(now);
        while let Some(event) = self.server.poll_event() {
            match event {
                ServerEvent::PeerJoined { peer, name, .. } => {
                    let k: usize = name[1..].parse().expect("name");
                    self.ids.insert(peer, k);
                }
                ServerEvent::PeerLeft { peer, reason } => {
                    if let Some(&k) = self.ids.get(&peer) {
                        self.left.entry(k).or_insert((now, reason));
                    }
                }
                _ => {}
            }
        }
        let tick_ms = 1_000 / u64::from(self.server.config().tick_rate_hz);
        for _ in 0..self.clock.advance(tick_ms) {
            self.server.advance_tick(|_, _| {});
        }
        self.server.flush(now);
        if self.hostile_from.is_some_and(|t| now >= t) {
            for row in self.server.peer_rows() {
                let player = self
                    .ids
                    .get(&PeerId(row.id))
                    .is_some_and(|&k| self.peers[k].role == Role::Player);
                if player {
                    self.rtt.push(row.rtt_ms);
                }
            }
        }
    }

    fn run_until(&mut self, until_ms: u64) {
        while self.now < until_ms {
            self.step();
        }
    }

    fn all_connected(&self) -> bool {
        self.peers.iter().all(|p| p.client.is_connected())
    }

    /// Missed input ticks over every player, now.
    fn players_missed(&self) -> u64 {
        self.server
            .peer_rows()
            .iter()
            .filter(|r| {
                self.ids
                    .get(&PeerId(r.id))
                    .is_some_and(|&k| self.peers[k].role == Role::Player)
            })
            .map(|r| r.missed_input_ticks)
            .sum()
    }

    fn players_left(&self) -> Vec<(usize, u64, DisconnectReason)> {
        self.left
            .iter()
            .filter(|(k, _)| self.peers[**k].role == Role::Player)
            .map(|(k, (t, r))| (*k, *t, *r))
            .collect()
    }

    fn rtt_p95(&self) -> f64 {
        let mut v = self.rtt.clone();
        assert!(!v.is_empty(), "no RTT samples");
        v.sort_by(f64::total_cmp);
        v[(v.len() * 95 / 100).min(v.len() - 1)]
    }
}

/// Joins everyone, warms up to `hostile_at`, then runs to `end`: (missed input ticks over the
/// measured window, server ticks in it, the rig).
fn flood_run(flood: bool, tick_rate_hz: u32, hostile_at: u64, end: u64) -> (u64, u64, Rig) {
    let mut roles = vec![Role::Player; 7];
    roles.push(if flood { Role::Flooder } else { Role::Player });
    let mut r = rig(&roles, tick_rate_hz, Some(hostile_at));
    r.run_until(3_000);
    assert!(r.all_connected(), "everyone joined");
    r.run_until(hostile_at);
    let (missed0, tick0) = (r.players_missed(), r.server.driver().tick());
    r.run_until(end);
    let (missed1, tick1) = (r.players_missed(), r.server.driver().tick());
    (missed1 - missed0, tick1 - tick0, r)
}

#[test]
fn a_100k_per_second_flooder_next_to_seven_players_is_removed_and_harms_no_one() {
    // A 20 Hz server: at 100k/s one tick's flood (5,000 datagrams) is more than the old
    // 4,096-datagram receive budget.
    flooder_next_to_seven_players(20);
}

#[test]
fn the_same_flood_at_the_default_30_hz_tick() {
    flooder_next_to_seven_players(30);
}

/// The flood profile at `tick_rate_hz`: the bars of spec 120 § 11.3.
fn flooder_next_to_seven_players(tick_rate_hz: u32) {
    const HOSTILE_AT: u64 = 5_000;
    const END: u64 = 11_000;
    let (control_missed, control_ticks, control) = flood_run(false, tick_rate_hz, HOSTILE_AT, END);
    let (flood_missed, flood_ticks, flood) = flood_run(true, tick_rate_hz, HOSTILE_AT, END);
    assert!(
        control.players_left().is_empty(),
        "control: no player left: {:?}",
        control.players_left()
    );
    assert!(
        flood.players_left().is_empty(),
        "flood: no player left: {:?}",
        flood.players_left()
    );
    // Seven players' inputs over the window (the eighth control slot is a player too, so the
    // control is counted over the same seven).
    let players = 7.0;
    let control_rate = control_missed as f64 / (control_ticks as f64 * (players + 1.0));
    let flood_rate = flood_missed as f64 / (flood_ticks as f64 * players);
    assert!(
        flood_rate <= control_rate + 0.01,
        "missed input ticks: flood {flood_rate:.4} ({flood_missed} of {flood_ticks} ticks x 7) vs control {control_rate:.4}"
    );
    let (control_p95, flood_p95) = (control.rtt_p95(), flood.rtt_p95());
    assert!(
        flood_p95 <= control_p95 + 20.0,
        "RTT p95: flood {flood_p95:.1} ms vs control {control_p95:.1} ms"
    );
    let flooder = 7;
    let left = flood.left.get(&flooder).copied();
    let (at, reason) = left.unwrap_or_else(|| {
        panic!(
            "the flooder was not removed ({} flood datagrams sent, server totals {:?})",
            flood.flood_sent,
            flood.server.totals()
        )
    });
    println!(
        "flood at {tick_rate_hz} Hz: removed after {} ms; missed {flood_rate:.4} vs control {control_rate:.4}; RTT p95 {flood_p95:.1} vs {control_p95:.1} ms; sent {}; totals {:?}",
        at - HOSTILE_AT,
        flood.flood_sent,
        flood.server.totals()
    );
    assert_eq!(reason, DisconnectReason::Flood, "removed for flooding");
    assert!(
        at - HOSTILE_AT <= 2_000,
        "the flooder was removed {} ms after the flood began (bar 2,000)",
        at - HOSTILE_AT
    );
    assert!(flood.flood_sent >= 200_000, "the flood ran: {}", flood.flood_sent);
    // The rate check runs before the open: most flood datagrams never cost an AEAD open.
    let opened = flood.server.totals().opened_in;
    assert!(
        opened * 4 < flood.flood_sent,
        "{opened} sealed datagrams opened for {} flood datagrams",
        flood.flood_sent
    );
    let _ = &flood.net;
}

/// One hostile peer of `role` next to one good-wifi player, hostile from 3 s: when it left, why,
/// and whether the player stayed.
fn hostile_run(role: Role, tick_rate_hz: u32, end: u64) -> (Option<(u64, DisconnectReason)>, Rig) {
    let mut r = rig(&[Role::Player, role], tick_rate_hz, Some(3_000));
    r.run_until(2_500);
    assert!(r.all_connected(), "both joined");
    r.run_until(end);
    (r.left.get(&1).copied(), r)
}

#[test]
fn bad_good_alternation_is_removed_within_a_minute() {
    let (left, r) = hostile_run(Role::Alternator, 30, 63_000);
    let (at, reason) = left.unwrap_or_else(|| {
        panic!(
            "60 s of bad/good alternation did not remove the peer (server totals {:?})",
            r.server.totals()
        )
    });
    println!("alternation: removed after {} ms ({reason:?})", at - 3_000);
    assert_eq!(reason, DisconnectReason::MalformedPacket);
    assert!(at - 3_000 <= 60_000, "removed after {} ms", at - 3_000);
    assert!(
        r.players_left().is_empty(),
        "the player stayed: {:?}",
        r.players_left()
    );
}

#[test]
fn soak_f3_malformed_stream_is_removed() {
    // One undecodable message in every packet (100 a second) plus a 60 KiB undecodable reliable
    // message each second: 1,010 refusals in 10 s, the soak's 629 at its rate.
    let (left, r) = hostile_run(Role::MalformedStream, 30, 33_000);
    let (at, reason) = left.unwrap_or_else(|| {
        panic!(
            "30 s of the F3 malformed stream did not remove the peer (server totals {:?})",
            r.server.totals()
        )
    });
    println!("F3: removed after {} ms ({reason:?})", at - 3_000);
    assert_eq!(reason, DisconnectReason::MalformedPacket);
    // 101 points a second less the 4-point leak: 1,024 points in about 10.6 s.
    assert!(
        at - 3_000 <= 15_000,
        "removed after {} ms (bar 15,000)",
        at - 3_000
    );
    assert!(
        r.players_left().is_empty(),
        "the player stayed: {:?}",
        r.players_left()
    );
}

#[test]
fn soak_f4_message_flood_is_removed_for_flood() {
    // The default 30 Hz server; 190,000 valid messages a second in full packets (about 1,500
    // datagrams a second, inside the packet rate).
    let (left, r) = hostile_run(Role::MessageFlood, 30, 8_000);
    let (at, reason) = left.unwrap_or_else(|| {
        panic!(
            "5 s of 190k messages/s did not remove the peer (server totals {:?})",
            r.server.totals()
        )
    });
    println!("F4: removed after {} ms ({reason:?})", at - 3_000);
    assert_eq!(reason, DisconnectReason::Flood);
    assert!(at - 3_000 <= 2_100, "removed after {} ms", at - 3_000);
    assert!(
        r.players_left().is_empty(),
        "the player stayed: {:?}",
        r.players_left()
    );
}

#[test]
fn an_honest_peer_at_its_allowance_bursting_3x_for_200_ms_stays() {
    let (left, r) = hostile_run(Role::Bursty, 20, 13_000);
    assert_eq!(
        left,
        None,
        "the bursty honest peer was removed (server totals {:?})",
        r.server.totals()
    );
    assert!(
        r.players_left().is_empty(),
        "the player stayed: {:?}",
        r.players_left()
    );
}
