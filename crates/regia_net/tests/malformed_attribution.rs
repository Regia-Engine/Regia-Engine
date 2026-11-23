// SPDX-License-Identifier: MPL-2.0
//! Who may score a peer's malformed points, and how fast a malformed stream goes (spec 120 § 11.2,
//! ADR-8181 amended by ADR-8606). In process: the in-memory network, seeded conditioners on the
//! clients, a 1 ms step clock, and a server socket that can inject datagrams under any source
//! address (an attacker spoofing an honest peer's address). Nothing sleeps and nothing reads the
//! wall clock.
//!
//! - The soak's malformed session (one undecodable message per 16 ms frame plus a 60 KiB
//!   undecodable reliable message each second) is removed within 2 s of its first bad message
//!   (the soak's bar is 5 s; ADR-8181's 1-point weight took about 17 s).
//! - A drip of 5 undecodable messages a second is removed within 15 s.
//! - An honest client on a lossy, duplicating, reordering link that sends one undecodable message
//!   a minute for 30 minutes is never removed, and the link itself causes no refusal.
//! - An off-path attacker spoofing an honest sealed peer's address with 1,000 garbage datagrams a
//!   second for 30 s (random bytes, forged records under the peer's token, damaged and replayed
//!   copies of its own records) neither removes nor degrades it: the peer's delivered messages,
//!   missed input ticks and inbound status equal a run without the attacker.
//! - The same forgeries at 20,000 a second, far over the peer's packet rate, do not get it removed
//!   for flooding (the rate check's tokens go back when a record fails its open).
//! - On a plain (loopback) link, packets under the peer's token from an address it never validated
//!   cannot score it.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};

use regia_net::LoopbackSocket;
use regia_net::prelude::*;
use regia_net::server::{MALFORMED_SCORE_LIMIT, MALFORMED_WEIGHT};

/// One clock step, milliseconds.
const STEP_MS: u64 = 1;
/// Players act every this many steps (100 Hz).
const CLIENT_EVERY_MS: u64 = 10;
/// The soak's client frame (`regia_net_soak::workload::FRAME_MS`).
const SOAK_FRAME_MS: u64 = 16;
/// Input lead over the client's estimate of the server tick.
const LEAD_TICKS: u64 = 3;
/// The default server tick rate.
const TICK_HZ: u32 = 30;

#[derive(Debug)]
struct Game;

#[derive(Debug, Clone, PartialEq)]
enum Msg {
    /// A valid message.
    Ping(u8),
    /// An unknown tag: undecodable at the server (`MessageMalformed`).
    Bad,
    /// An unknown tag followed by `n` bytes: a large undecodable reliable message.
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
    const GAME_ID: &'static str = "regia.test.malformed_attribution";
    const PROTOCOL_VERSION: u32 = 1;
}

/// What the attacker injects and what the server received, shared with the test.
#[derive(Debug, Default)]
struct Wire {
    /// Datagrams to hand the server, with the source address they claim.
    injected: VecDeque<(Vec<u8>, SocketAddr)>,
    /// Every datagram the server read from `tapped_from`, newest last (what an on-path observer
    /// sees; at most `TAP_KEEP`).
    tapped: VecDeque<Vec<u8>>,
    tapped_from: Option<SocketAddr>,
    /// Alternates the injected and the real queue so forged and genuine datagrams interleave.
    turn: bool,
}

const TAP_KEEP: usize = 64;

/// The server's socket: the in-memory network plus injected datagrams under forged sources.
#[derive(Debug)]
struct SpoofableSocket {
    inner: LoopbackSocket,
    wire: Arc<Mutex<Wire>>,
}

impl DatagramSocket for SpoofableSocket {
    fn send_to(&mut self, bytes: &[u8], dest: SocketAddr) -> std::io::Result<()> {
        self.inner.send_to(bytes, dest)
    }

    fn recv_from(&mut self, buf: &mut [u8]) -> std::io::Result<Option<(usize, SocketAddr)>> {
        let mut wire = self.wire.lock().unwrap_or_else(PoisonError::into_inner);
        wire.turn = !wire.turn;
        if wire.turn
            && let Some((bytes, from)) = wire.injected.pop_front()
        {
            let n = bytes.len().min(buf.len());
            buf[..n].copy_from_slice(&bytes[..n]);
            return Ok(Some((n, from)));
        }
        if let Some((n, from)) = self.inner.recv_from(buf)? {
            if wire.tapped_from == Some(from) {
                if wire.tapped.len() >= TAP_KEEP {
                    wire.tapped.pop_front();
                }
                wire.tapped.push_back(buf[..n].to_vec());
            }
            return Ok(Some((n, from)));
        }
        let Some((bytes, from)) = wire.injected.pop_front() else {
            return Ok(None);
        };
        let n = bytes.len().min(buf.len());
        buf[..n].copy_from_slice(&bytes[..n]);
        Ok(Some((n, from)))
    }

    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.inner.local_addr()
    }
}

type Server = NetServer<Game, SpoofableSocket>;
type Client = NetClient<Game, LoopbackSocket>;

/// A deterministic generator (SplitMix64).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

/// The good-wifi profile of the soak harness, one direction.
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

/// A bad mobile link, one direction: 2 % loss, 1 % duplication, 2 % reordering up to 40 ms,
/// 30 ms + 0-20 ms jitter.
fn lossy_jittery(seed: u64) -> ConditionerConfig {
    ConditionerConfig {
        enabled: true,
        seed,
        loss: 0.02,
        duplicate: 0.01,
        reorder: 0.02,
        reorder_max_ms: 40,
        delay_ms: 30,
        jitter_ms: 20,
        ..ConditionerConfig::default()
    }
}

/// What a peer sends besides its inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    /// One input per tick and a valid message every 100 ms.
    Player,
    /// The soak's `malformed_messages` session: one undecodable message per 16 ms frame and a
    /// 60 KiB undecodable reliable message each second.
    SoakMalformed,
    /// One undecodable message every 200 ms (5 a second).
    Drip,
    /// A player that also sends one undecodable reliable message a minute.
    HonestOneAMinute,
}

struct Peer {
    client: Client,
    role: Role,
    last_input: Option<u64>,
    /// Undecodable messages this peer sent, and when the first one left.
    bad_sent: u64,
    first_bad_ms: Option<u64>,
}

struct Rig {
    _net: LoopbackNet,
    wire: Arc<Mutex<Wire>>,
    server: Server,
    peers: Vec<Peer>,
    clock: TickClock,
    now: u64,
    /// When each peer (by index) left, and why.
    left: Vec<Option<(u64, DisconnectReason)>>,
    ids: Vec<Option<PeerId>>,
    /// Valid messages the server delivered, per peer index.
    delivered: Vec<u64>,
    /// When the role's hostile or bad sends start.
    bad_from: u64,
    /// The highest malformed score seen per peer index (sampled after each server tick).
    max_points: Vec<u64>,
}

fn rig(server_addr: SocketAddr, peers: &[(Role, SocketAddr, ConditionerConfig)], bad_from: u64) -> Rig {
    let net = LoopbackNet::new();
    let wire = Arc::new(Mutex::new(Wire::default()));
    let config = ServerConfig {
        tick_rate_hz: TICK_HZ,
        ..ServerConfig::default()
    };
    let socket = SpoofableSocket {
        inner: net.bind(server_addr).expect("bind server"),
        wire: wire.clone(),
    };
    let server = NetServer::with_secret(socket, config, [7; 32]);
    let peers: Vec<Peer> = peers
        .iter()
        .enumerate()
        .map(|(k, (role, addr, conditioner))| {
            let config = ClientConfig {
                name: format!("p{k}"),
                conditioner: conditioner.clone(),
                ..ClientConfig::default()
            };
            let socket = net.bind(*addr).expect("bind client");
            Peer {
                client: NetClient::connect(socket, server_addr, config, &()).expect("join fits"),
                role: *role,
                last_input: None,
                bad_sent: 0,
                first_bad_ms: None,
            }
        })
        .collect();
    let n = peers.len();
    Rig {
        _net: net,
        wire,
        server,
        peers,
        clock: TickClock::new(TICK_HZ, 4),
        now: 0,
        left: vec![None; n],
        ids: vec![None; n],
        delivered: vec![0; n],
        bad_from,
        max_points: vec![0; n],
    }
}

impl Rig {
    /// One 1 ms step: clients act on their schedules, the server ticks at 30 Hz.
    fn step(&mut self) {
        self.now += STEP_MS;
        let now = self.now;
        let bad = now >= self.bad_from;
        for peer in &mut self.peers {
            let frame = match peer.role {
                Role::SoakMalformed => SOAK_FRAME_MS,
                _ => CLIENT_EVERY_MS,
            };
            if !now.is_multiple_of(frame) {
                continue;
            }
            peer.client.receive(now);
            if peer.client.is_connected() {
                let target = peer.client.estimated_server_tick(now) + LEAD_TICKS;
                let from = peer.last_input.map_or(target, |t| t + 1);
                for tick in from..=target {
                    let _ = peer.client.send_input(tick, Move(1));
                }
                if from <= target {
                    peer.last_input = Some(target);
                }
                if now.is_multiple_of(100) {
                    let _ = peer.client.send(Channel::Unreliable, &Msg::Ping(1));
                }
                let mut sent_bad = 0;
                if bad {
                    match peer.role {
                        Role::Player => {}
                        Role::SoakMalformed => {
                            if peer.client.send(Channel::Unreliable, &Msg::Bad).is_ok() {
                                sent_bad += 1;
                            }
                            if now.is_multiple_of(SOAK_FRAME_MS * 60)
                                && peer.client.send(Channel::GAME_RPC, &Msg::BadBlob(60_000)).is_ok()
                            {
                                sent_bad += 1;
                            }
                        }
                        Role::Drip => {
                            if now.is_multiple_of(200)
                                && peer.client.send(Channel::Unreliable, &Msg::Bad).is_ok()
                            {
                                sent_bad += 1;
                            }
                        }
                        Role::HonestOneAMinute => {
                            if now.is_multiple_of(60_000)
                                && peer.client.send(Channel::GAME_RPC, &Msg::Bad).is_ok()
                            {
                                sent_bad += 1;
                            }
                        }
                    }
                }
                if sent_bad > 0 {
                    peer.bad_sent += sent_bad;
                    peer.first_bad_ms.get_or_insert(now);
                }
            }
            peer.client.flush(now);
            while peer.client.poll_event().is_some() {}
        }
        let tick_ms = 1_000 / u64::from(TICK_HZ);
        if now.is_multiple_of(tick_ms) {
            self.server_tick();
        }
    }

    fn server_tick(&mut self) {
        let now = self.now;
        self.server.receive(now);
        while let Some(event) = self.server.poll_event() {
            match event {
                ServerEvent::PeerJoined { peer, name, .. } => {
                    let k: usize = name[1..].parse().expect("name");
                    self.ids[k] = Some(peer);
                }
                ServerEvent::PeerLeft { peer, reason } => {
                    if let Some(k) = self.index(peer) {
                        self.left[k].get_or_insert((now, reason));
                    }
                }
                ServerEvent::Message {
                    peer,
                    message: Msg::Ping(_),
                    ..
                } => {
                    if let Some(k) = self.index(peer) {
                        self.delivered[k] += 1;
                    }
                }
                _ => {}
            }
        }
        let tick_ms = 1_000 / u64::from(TICK_HZ);
        for _ in 0..self.clock.advance(tick_ms) {
            self.server.advance_tick(|_, _| {});
        }
        self.server.flush(now);
        for k in 0..self.peers.len() {
            if let Some(status) = self.ids[k].and_then(|p| self.server.peer_inbound(p)) {
                self.max_points[k] = self.max_points[k].max(status.malformed_points);
            }
        }
    }

    fn index(&self, peer: PeerId) -> Option<usize> {
        self.ids.iter().position(|id| *id == Some(peer))
    }

    fn run_until(&mut self, until_ms: u64) {
        while self.now < until_ms {
            self.step();
        }
    }

    fn all_connected(&self) -> bool {
        self.peers.iter().all(|p| p.client.is_connected())
    }

    fn missed(&self, k: usize) -> u64 {
        let Some(id) = self.ids[k] else { return 0 };
        self.server
            .peer_rows()
            .iter()
            .find(|r| PeerId(r.id) == id)
            .map_or(0, |r| r.missed_input_ticks)
    }

    fn inbound(&self, k: usize) -> Option<regia_net::inbound::PeerInboundStatus> {
        self.ids[k].and_then(|p| self.server.peer_inbound(p))
    }

    /// How long after its first bad send peer `k` left, and why (panics when it stayed).
    fn removal(&self, k: usize) -> (u64, DisconnectReason) {
        let (at, reason) = self.left[k].unwrap_or_else(|| {
            panic!(
                "peer {k} ({:?}) was not removed after {} bad messages (server totals {:?})",
                self.peers[k].role,
                self.peers[k].bad_sent,
                self.server.totals()
            )
        });
        let first = self.peers[k].first_bad_ms.expect("the peer sent a bad message");
        (at - first, reason)
    }
}

fn server_addr() -> SocketAddr {
    SocketAddr::from(([10, 0, 0, 1], 7777))
}

/// Non-loopback client addresses: `CryptoMode::Auto` seals every link.
fn sealed_addr(k: u8) -> SocketAddr {
    SocketAddr::from(([10, 0, 1, k + 1], 5000))
}

#[test]
fn the_soak_malformed_session_is_removed_within_two_seconds() {
    // The soak's own session: 62.5 undecodable messages a second plus one 60 KiB undecodable
    // reliable message each second, next to a good-wifi player. At 16 points each the score
    // passes 1,024 after 64 refusals, about 1 s; the soak allows 5 s (HOSTILE_DROP_MS) for the
    // removal and the hostile client hearing it.
    let mut r = rig(
        server_addr(),
        &[
            (Role::Player, sealed_addr(0), good_wifi(100)),
            (Role::SoakMalformed, sealed_addr(1), good_wifi(101)),
        ],
        3_000,
    );
    r.run_until(2_500);
    assert!(r.all_connected(), "both joined");
    r.run_until(23_000);
    let (after, reason) = r.removal(1);
    println!("soak malformed session: removed {after} ms after its first bad message ({reason:?})");
    assert_eq!(reason, DisconnectReason::MalformedPacket);
    assert!(
        after <= 2_000,
        "removed {after} ms after the first bad message (bar 2,000)"
    );
    assert!(
        !r.peers[1].client.is_connected(),
        "the hostile client heard its removal"
    );
    assert_eq!(r.left[0], None, "the player stayed");
    assert_eq!(r.max_points[0], 0, "the player was never scored");
}

#[test]
fn a_drip_of_five_malformed_messages_a_second_is_removed_within_15_s() {
    // 5 x 16 = 80 points a second less the 4-point leak: 1,024 points after about 13.5 s.
    let mut r = rig(
        server_addr(),
        &[
            (Role::Player, sealed_addr(0), good_wifi(100)),
            (Role::Drip, sealed_addr(1), good_wifi(101)),
        ],
        3_000,
    );
    r.run_until(2_500);
    assert!(r.all_connected(), "both joined");
    r.run_until(23_000);
    let (after, reason) = r.removal(1);
    println!("drip of 5/s: removed {after} ms after its first bad message ({reason:?})");
    assert_eq!(reason, DisconnectReason::MalformedPacket);
    assert!(
        after <= 15_000,
        "removed {after} ms after the first bad message (bar 15,000)"
    );
    // Not removed before the arithmetic allows: 64 refusals at the least.
    assert!(
        r.peers[1].bad_sent >= u64::from(MALFORMED_SCORE_LIMIT / MALFORMED_WEIGHT),
        "{} bad messages",
        r.peers[1].bad_sent
    );
    assert_eq!(r.left[0], None, "the player stayed");
}

#[test]
fn an_honest_client_with_one_malformed_message_a_minute_for_30_minutes_stays() {
    // 30 minutes simulated on a lossy, duplicating, reordering link: 30 undecodable reliable
    // messages (a rare game bug), each scoring 16 points that leak away in 4 s.
    const END: u64 = 30 * 60_000 + 2_000;
    let mut r = rig(
        server_addr(),
        &[(Role::HonestOneAMinute, sealed_addr(0), lossy_jittery(7))],
        0,
    );
    r.run_until(3_000);
    assert!(r.all_connected(), "joined");
    let malformed_at_join = r.server.totals().malformed;
    r.run_until(END);
    assert_eq!(r.left[0], None, "the honest client stayed for 30 minutes");
    assert!(r.peers[0].client.is_connected());
    assert_eq!(r.peers[0].bad_sent, 30, "one a minute");
    assert_eq!(
        r.server.totals().malformed - malformed_at_join,
        30,
        "only the 30 bad messages were refused: loss, duplication and reordering cost nothing"
    );
    assert!(
        r.max_points[0] <= u64::from(MALFORMED_WEIGHT),
        "the score never held more than one refusal: {}",
        r.max_points[0]
    );
    let status = r.inbound(0).expect("connected");
    assert_eq!(status.flood_ms, 0);
    assert!(r.delivered[0] > 0);
}

/// What the spoofer sends, from the honest peer's own address.
#[derive(Debug, Clone, Copy)]
struct Spoof {
    per_s: u64,
    from_ms: u64,
    until_ms: u64,
    /// Forgeries built from the peer's tapped records (its token, damaged or replayed). Without,
    /// only random bytes (an attacker who never saw the peer's traffic).
    tap: bool,
}

/// One forged datagram: random bytes, a random record under the peer's tapped header, a damaged
/// copy of a tapped record (its tag fails), or an exact replay.
fn forge(rng: &mut Rng, tapped: &VecDeque<Vec<u8>>, tap: bool) -> Vec<u8> {
    let pick = if tap && !tapped.is_empty() {
        rng.below(4)
    } else {
        0
    };
    let base = |rng: &mut Rng| tapped[rng.below(tapped.len() as u64) as usize].clone();
    match pick {
        0 => {
            let len = 1 + rng.below(1_200) as usize;
            (0..len).map(|_| rng.next() as u8).collect()
        }
        1 => {
            // The sealed header's prefix (magic, kind, token) and random ciphertext.
            let mut d = base(rng);
            let keep = d.len().min(9);
            for b in &mut d[keep..] {
                *b = rng.next() as u8;
            }
            d
        }
        2 => {
            let mut d = base(rng);
            let at = 9 + rng.below((d.len().saturating_sub(9)).max(1) as u64) as usize;
            if let Some(b) = d.get_mut(at) {
                *b ^= 1 << rng.below(8);
            }
            d
        }
        _ => base(rng),
    }
}

/// Two sealed good-wifi players; peer 0 is the victim. Runs to `end` with `spoof` against it:
/// (the rig, forged datagrams sent).
fn spoof_run(spoof: Option<Spoof>, end: u64) -> (Rig, u64) {
    let victim = sealed_addr(0);
    let mut r = rig(
        server_addr(),
        &[
            (Role::Player, victim, good_wifi(100)),
            (Role::Player, sealed_addr(1), good_wifi(101)),
        ],
        u64::MAX,
    );
    r.wire.lock().unwrap_or_else(PoisonError::into_inner).tapped_from = Some(victim);
    let mut rng = Rng(0x5EED);
    let mut sent = 0u64;
    while r.now < end {
        r.step();
        let Some(s) = spoof else { continue };
        if !(s.from_ms..s.until_ms).contains(&r.now) {
            continue;
        }
        // `per_s` spread evenly over the milliseconds.
        let due = s.per_s * (r.now - s.from_ms + 1) / 1_000;
        let mut wire = r.wire.lock().unwrap_or_else(PoisonError::into_inner);
        while sent < due {
            let d = forge(&mut rng, &wire.tapped, s.tap);
            wire.injected.push_back((d, victim));
            sent += 1;
        }
    }
    (r, sent)
}

#[test]
fn an_off_path_spoofer_cannot_drop_or_degrade_an_honest_peer() {
    // 1,000 garbage datagrams a second for 30 s under the victim's address, half of them built
    // from its own records (its token, damaged, replayed): the worst an observer could send.
    const END: u64 = 35_000;
    let spoof = Spoof {
        per_s: 1_000,
        from_ms: 3_000,
        until_ms: 33_000,
        tap: true,
    };
    let (control, _) = spoof_run(None, END);
    let (attacked, sent) = spoof_run(Some(spoof), END);
    println!(
        "spoof 1,000/s: {sent} forged; totals {:?}; control delivered {:?} missed {} / attacked delivered {:?} missed {}",
        attacked.server.totals(),
        control.delivered,
        control.missed(0),
        attacked.delivered,
        attacked.missed(0)
    );
    assert!(sent >= 29_000, "the spoofer ran: {sent}");
    assert_eq!(attacked.left, vec![None, None], "no one left");
    let status = attacked.inbound(0).expect("the victim is connected");
    assert_eq!(status.malformed_points, 0, "never scored");
    assert_eq!(status.flood_ms, 0, "never over rate");
    assert_eq!(
        status,
        control.inbound(0).expect("control victim"),
        "the victim's inbound status is the control's"
    );
    assert_eq!(
        attacked.delivered, control.delivered,
        "every valid message arrived as in the control"
    );
    assert_eq!(
        attacked.missed(0),
        control.missed(0),
        "missed input ticks as in the control"
    );
    assert_eq!(attacked.max_points, vec![0, 0]);
}

#[test]
fn forged_records_over_the_peers_packet_rate_do_not_remove_it_for_flood() {
    // 20,000 forged records a second under the victim's address and token (its packet rate is
    // 4 x 64 x 30 = 7,680 a second): before ADR-8606 they spent its rate and its flood timer
    // removed it within 2 s.
    const END: u64 = 15_000;
    let spoof = Spoof {
        per_s: 20_000,
        from_ms: 3_000,
        until_ms: 13_000,
        tap: true,
    };
    let (control, _) = spoof_run(None, END);
    let (attacked, sent) = spoof_run(Some(spoof), END);
    println!(
        "spoof 20,000/s: {sent} forged; totals {:?}; missed control {} attacked {}",
        attacked.server.totals(),
        control.missed(0),
        attacked.missed(0)
    );
    assert!(sent >= 190_000, "the spoofer ran: {sent}");
    assert_eq!(attacked.left, vec![None, None], "no one left");
    let status = attacked.inbound(0).expect("the victim is connected");
    assert_eq!(status.flood_ms, 0, "forged records never fed the flood timer");
    assert_eq!(status.malformed_points, 0);
    assert_eq!(
        status.over_rate_drops, 0,
        "no genuine datagram was dropped for rate"
    );
    assert_eq!(
        attacked.delivered, control.delivered,
        "every valid message arrived as in the control"
    );
    assert_eq!(
        attacked.missed(0),
        control.missed(0),
        "missed input ticks as in the control"
    );
}

#[test]
fn an_unvalidated_address_cannot_score_a_plain_link_peer() {
    // Loopback: `CryptoMode::Auto` leaves the link plain, so the token (visible on the wire) and a
    // packet number newer than any received are all a packet from a new address shows. 1,000 a
    // second of such packets carrying an unparseable record, each from its own port, for 10 s.
    let server = SocketAddr::from(([127, 0, 0, 1], 7777));
    let victim = SocketAddr::from(([127, 0, 0, 2], 5000));
    let mut r = rig(
        server,
        &[(Role::Player, victim, ConditionerConfig::default())],
        u64::MAX,
    );
    r.wire.lock().unwrap_or_else(PoisonError::into_inner).tapped_from = Some(victim);
    r.run_until(3_000);
    assert!(r.all_connected(), "joined");
    let header: Vec<u8> = {
        let wire = r.wire.lock().unwrap_or_else(PoisonError::into_inner);
        let d = wire.tapped.back().expect("the victim sent something");
        d[..9].to_vec()
    };
    let mut sent = 0u64;
    while r.now < 13_000 {
        r.step();
        let mut wire = r.wire.lock().unwrap_or_else(PoisonError::into_inner);
        let mut d = header.clone();
        // Flags: a 4-byte packet number, far ahead of the victim's; then an unknown record tag.
        d.push(0x02);
        d.extend_from_slice(&(100_000u32 + sent as u32).to_le_bytes());
        d.push(0xEE);
        let from = SocketAddr::from(([127, 0, 0, 9], 20_000 + (sent % 40_000) as u16));
        wire.injected.push_back((d, from));
        sent += 1;
    }
    r.run_until(14_000);
    println!(
        "plain candidate forgeries: {sent}; totals {:?}",
        r.server.totals()
    );
    assert_eq!(r.server.totals().opened_in, 0, "the link is plain");
    // KI-NT-018 (ADR-10077): the forgeries arrive, but a plain packet from an address that never
    // answered a probe is held, not applied, so none reaches the connection (the base asserted
    // `malformed >= 1_000` here: each forgery was applied and refused there).
    assert!(
        r.server.totals().datagrams_in >= sent,
        "the forgeries arrived: {:?}",
        r.server.totals()
    );
    assert_eq!(
        r.server.totals().malformed,
        0,
        "no forgery from an unvalidated address was applied: {:?}",
        r.server.totals()
    );
    assert_eq!(r.left[0], None, "the victim stayed");
    assert!(r.peers[0].client.is_connected());
    assert_eq!(r.max_points[0], 0, "never scored");
}
