// SPDX-License-Identifier: MPL-2.0
//! Path validation of address changes (ADR-8299, spec 120 § 10.4; RFC 9000 §§ 8-9), end to end over
//! the in-memory network. Virtual time: one step is 10 ms, nothing sleeps.
//!
//! Every client sits behind its own [`Nat`]: the server hears the NAT's current public address,
//! and a real NAT forgets an old mapping, so the server's traffic to a public address the client
//! no longer uses is dropped. A [`Racer`] is an on-path observer that copies every datagram it sees
//! and sends the copy from its own address so it arrives first (soak finding F1, adversarial row
//! 11). The bar `no_hijack` (spec 120 § 15.2) is checked as: no peer ever moves to an address its
//! client does not hold.

mod common;

use std::collections::BTreeMap;
use std::net::SocketAddr;

use common::{Duel, DuelMsg, Team};
use regia_net::crypto::SEALED_HEADER_BYTES;
use regia_net::handshake::{PacketKind, split_datagram};
use regia_net::path::{ANTI_AMPLIFICATION_FACTOR, PATH_PROBE_RESEND_MS};
use regia_net::prelude::*;
use regia_net::server::REBIND_ATTEMPTS_PER_S;
use regia_net::{CryptoMode, LoopbackSocket, PeerRow};

const STEP_MS: u64 = 10;
const SERVER: ([u8; 4], u16) = ([10, 0, 0, 1], 7777);
const ATTACKER: ([u8; 4], u16) = ([198, 51, 100, 66], 6666);

fn sa(a: ([u8; 4], u16)) -> SocketAddr {
    SocketAddr::from(a)
}

/// An on-path observer that copies what it sees and races the copy from its own address.
struct Racer {
    socket: LoopbackSocket,
    /// Bytes the racer sent (its copies).
    sent: u64,
    /// Bytes the racer received (the endpoint's probes, anything sent to its address).
    received: u64,
}

impl Racer {
    fn drain(&mut self) {
        let mut buf = [0u8; 2048];
        while let Some((n, _)) = self.socket.recv_from(&mut buf).expect("io") {
            self.received += n as u64;
        }
    }
}

/// A client-side NAT: the client talks to `inside`; the server hears `outside[current]`.
struct Nat {
    inside: LoopbackSocket,
    outside: Vec<LoopbackSocket>,
    outside_addrs: Vec<SocketAddr>,
    current: usize,
    client: SocketAddr,
    server: SocketAddr,
    /// Copies every upstream datagram and sends it first from its own address.
    racer: Option<Racer>,
    /// Every client-to-server datagram.
    upstream: Vec<Vec<u8>>,
}

impl Nat {
    fn pump(&mut self) {
        let mut buf = [0u8; 2048];
        while let Some((n, _)) = self.inside.recv_from(&mut buf).expect("io") {
            self.upstream.push(buf[..n].to_vec());
            if let Some(r) = self.racer.as_mut() {
                r.socket.send_to(&buf[..n], self.server).expect("send");
                r.sent += n as u64;
            }
            self.outside[self.current]
                .send_to(&buf[..n], self.server)
                .expect("send");
        }
        for (i, socket) in self.outside.iter_mut().enumerate() {
            while let Some((n, _)) = socket.recv_from(&mut buf).expect("io") {
                // A real NAT forgot the old mapping: the server's traffic there is lost.
                if i == self.current {
                    self.inside.send_to(&buf[..n], self.client).expect("send");
                }
            }
        }
        if let Some(r) = self.racer.as_mut() {
            r.drain();
        }
    }
}

struct Player {
    client: NetClient<Duel, LoopbackSocket>,
    nat: Nat,
    /// Chats the server received from this player, in order.
    up: Vec<String>,
    /// Chats this player received, in order.
    down: Vec<String>,
    /// Steps since this player last received a chat, and the longest such stretch.
    quiet: u64,
    longest_quiet: u64,
    ended: Option<ClientEnd>,
}

struct World {
    net: LoopbackNet,
    server: NetServer<Duel, LoopbackSocket>,
    players: Vec<Player>,
    now: u64,
    left: Vec<(PeerId, DisconnectReason)>,
    /// Every address a peer was seen at, to check `no_hijack`.
    seen_addrs: BTreeMap<PeerId, Vec<SocketAddr>>,
}

fn bad_wifi(seed: u64) -> ConditionerConfig {
    // The soak harness's bad-wifi (8 ms + 0-30 ms jitter, 1 % random loss plus bursts, about 3 %
    // in total, 1 % reorder within 30 ms, 0.5 % duplicates), in the crate's conditioner.
    ConditionerConfig {
        enabled: true,
        seed,
        loss: 0.03,
        duplicate: 0.005,
        reorder: 0.01,
        reorder_max_ms: 30,
        delay_ms: 8,
        jitter_ms: 30,
        ..ConditionerConfig::default()
    }
}

impl World {
    fn new(server_config: ServerConfig) -> Self {
        let net = LoopbackNet::new();
        let server = NetServer::with_secret(net.bind(sa(SERVER)).expect("bind"), server_config, [7; 32]);
        Self {
            net,
            server,
            players: Vec::new(),
            now: 0,
            left: Vec::new(),
            seen_addrs: BTreeMap::new(),
        }
    }

    /// Adds a client at `client` behind a NAT with the public addresses `outside` (the first is
    /// used first) and connects it.
    fn join(&mut self, client: SocketAddr, outside: &[SocketAddr], config: ClientConfig) -> usize {
        let inside_addr = SocketAddr::new(client.ip(), client.port().wrapping_add(10_000));
        let c = NetClient::connect(
            self.net.bind(client).expect("bind"),
            inside_addr,
            config,
            &Team(1),
        )
        .expect("join fits");
        let nat = Nat {
            inside: self.net.bind(inside_addr).expect("bind"),
            outside: outside.iter().map(|a| self.net.bind(*a).expect("bind")).collect(),
            outside_addrs: outside.to_vec(),
            current: 0,
            client,
            server: sa(SERVER),
            racer: None,
            upstream: Vec::new(),
        };
        self.players.push(Player {
            client: c,
            nat,
            up: Vec::new(),
            down: Vec::new(),
            quiet: 0,
            longest_quiet: 0,
            ended: None,
        });
        let i = self.players.len() - 1;
        for _ in 0..300 {
            if self.players[i].client.is_connected() {
                break;
            }
            self.step();
        }
        assert!(
            self.players[i].client.is_connected(),
            "{:?}",
            self.players[i].client.state()
        );
        for p in &mut self.players {
            p.longest_quiet = 0;
            p.quiet = 0;
        }
        i
    }

    fn peer(&self, i: usize) -> PeerId {
        self.players[i].client.peer_id().expect("connected")
    }

    fn peer_addr(&self, i: usize) -> SocketAddr {
        self.server.session().peer(self.peer(i)).expect("peer").addr
    }

    fn row(&self, i: usize) -> PeerRow {
        let id = self.peer(i).0;
        self.server
            .peer_rows()
            .into_iter()
            .find(|r| r.id == id)
            .expect("row")
    }

    fn step(&mut self) {
        self.now += STEP_MS;
        for p in &mut self.players {
            p.client.update(self.now);
            p.nat.pump();
        }
        self.server.update(self.now);
        for p in &mut self.players {
            p.nat.pump();
        }
        let by_peer: BTreeMap<PeerId, usize> = self
            .players
            .iter()
            .enumerate()
            .filter_map(|(i, p)| p.client.peer_id().map(|id| (id, i)))
            .collect();
        while let Some(e) = self.server.poll_event() {
            match e {
                ServerEvent::Message {
                    peer,
                    message: DuelMsg::Chat(text),
                    ..
                } => {
                    if let Some(&i) = by_peer.get(&peer) {
                        self.players[i].up.push(text);
                    }
                }
                ServerEvent::PeerLeft { peer, reason } => self.left.push((peer, reason)),
                _ => {}
            }
        }
        for peer in self.server.session().peers() {
            let seen = self.seen_addrs.entry(peer.id).or_default();
            if seen.last() != Some(&peer.addr) {
                seen.push(peer.addr);
            }
        }
        for p in &mut self.players {
            p.quiet += 1;
            while let Some(e) = p.client.poll_event() {
                match e {
                    ClientEvent::Message {
                        message: DuelMsg::Chat(text),
                        ..
                    } => {
                        p.down.push(text);
                        p.quiet = 0;
                    }
                    ClientEvent::Ended(why) => p.ended = Some(why),
                    _ => {}
                }
            }
            p.longest_quiet = p.longest_quiet.max(p.quiet);
        }
    }

    /// One reliable chat each way for player `i`, numbered `n`.
    fn chat(&mut self, i: usize, n: usize) {
        let text = format!("p{i}-m{n:05}-{}", "x".repeat(100));
        self.players[i]
            .client
            .send(Channel::GAME_RPC, &DuelMsg::Chat(text.clone()))
            .expect("queued");
        let peer = self.peer(i);
        self.server
            .send(peer, Channel::GAME_RPC, &DuelMsg::Chat(text))
            .expect("queued");
    }

    /// Steps `steps` times; every player chats once per step, numbered from `first`.
    fn run_chatting(&mut self, first: usize, steps: usize) {
        for n in first..first + steps {
            for i in 0..self.players.len() {
                self.chat(i, n);
            }
            self.step();
        }
    }

    fn settle(&mut self, steps: usize) {
        for _ in 0..steps {
            self.step();
        }
    }

    /// Asserts player `i` got and sent every chat numbered `0..n` exactly once, in order.
    fn assert_all_chats(&self, i: usize, n: usize) {
        let expected: Vec<String> = (0..n)
            .map(|k| format!("p{i}-m{k:05}-{}", "x".repeat(100)))
            .collect();
        assert_eq!(self.players[i].up.len(), n, "upstream chats of player {i}");
        assert_eq!(self.players[i].up, expected, "upstream chats of player {i}");
        assert_eq!(self.players[i].down.len(), n, "downstream chats of player {i}");
        assert_eq!(self.players[i].down, expected, "downstream chats of player {i}");
    }

    /// `no_hijack`: every address peer `i` was ever at is one of its NAT's public addresses.
    fn assert_no_hijack(&self, i: usize) {
        let own = &self.players[i].nat.outside_addrs;
        for addr in self.seen_addrs.get(&self.peer(i)).into_iter().flatten() {
            assert!(
                own.contains(addr),
                "peer {i} moved to {addr}, which its client never held"
            );
        }
    }
}

fn client_config(name: &str) -> ClientConfig {
    ClientConfig {
        name: name.into(),
        ..ClientConfig::default()
    }
}

fn public(last: u8, port: u16) -> SocketAddr {
    SocketAddr::from(([203, 0, 113, last], port))
}

fn one_player() -> World {
    let mut w = World::new(ServerConfig::default());
    w.join(
        SocketAddr::from(([192, 168, 0, 2], 5000)),
        &[public(5, 40_000), public(5, 40_001), public(9, 50_000)],
        client_config("ada"),
    );
    w
}

/// F1 / spoof race (acceptance): an on-path observer copies every fresh record of the player and
/// races it from a third address. The peer stays on its own address, every chat arrives once each
/// way, the third address gets at most three times the bytes it sent, and the player is never
/// cut off for longer than one round trip plus a probe interval.
#[test]
fn a_copy_raced_from_a_third_address_never_moves_the_peer() {
    let mut w = one_player();
    w.players[0].nat.racer = Some(Racer {
        socket: w.net.bind(sa(ATTACKER)).expect("bind"),
        sent: 0,
        received: 0,
    });
    w.run_chatting(0, 300);
    // While chats flow (the idle settle below is quiet by design).
    let longest_quiet = w.players[0].longest_quiet;
    w.settle(100);
    assert_eq!(w.peer_addr(0), public(5, 40_000), "the peer never moved");
    assert_eq!(w.server.totals().rebinds, 0, "{:?}", w.server.totals());
    w.assert_all_chats(0, 300);
    w.assert_no_hijack(0);
    let racer = w.players[0].nat.racer.as_ref().expect("racer");
    assert!(racer.sent > 0);
    assert!(
        racer.received <= ANTI_AMPLIFICATION_FACTOR * racer.sent,
        "third address got {} bytes for {} sent",
        racer.received,
        racer.sent
    );
    // One round trip here is two steps; a probe interval is `PATH_PROBE_RESEND_MS`.
    let bound = 2 + PATH_PROBE_RESEND_MS / STEP_MS;
    assert!(
        longest_quiet <= bound,
        "the player went {longest_quiet} steps without a chat (bound {bound})"
    );
    let row = w.row(0);
    assert!(row.path_probes > 0, "the third address was probed: {row:?}");
    assert_eq!(row.rebinds, 0);
}

/// Adversarial row 11: one fresh datagram arrives first from a second address, then the
/// original from the peer's own address. The address stays, and the message is delivered once.
#[test]
fn a_fresh_datagram_first_from_a_second_address_then_the_original() {
    let mut w = one_player();
    w.run_chatting(0, 3);
    w.settle(20);
    let mut attacker = w.net.bind(sa(ATTACKER)).expect("bind");
    // The player's next datagram, held at the NAT.
    let text = format!("p0-m{:05}-{}", 3, "x".repeat(100));
    w.players[0]
        .client
        .send(Channel::GAME_RPC, &DuelMsg::Chat(text))
        .expect("queued");
    w.now += STEP_MS;
    w.players[0].client.flush(w.now);
    let mut buf = [0u8; 2048];
    let (n, _) = w.players[0]
        .nat
        .inside
        .recv_from(&mut buf)
        .expect("io")
        .expect("a datagram");
    let datagram = buf[..n].to_vec();
    assert!(split_datagram(&datagram).is_some_and(|(k, _)| k == PacketKind::Sealed));
    attacker.send_to(&datagram, sa(SERVER)).expect("send");
    w.players[0].nat.outside[0]
        .send_to(&datagram, sa(SERVER))
        .expect("send");
    w.server.receive(w.now);
    assert_eq!(w.peer_addr(0), public(5, 40_000), "the address stays");
    // Drain the server's events into the world's bookkeeping, then let the chat finish.
    w.settle(1);
    // Chat 3 was the held one: the server got it once. Chat each way for the rest.
    for i in 4..10 {
        w.chat(0, i);
        w.step();
    }
    // The downstream copy of chat 3 has to be sent too for the comparison.
    let peer = w.peer(0);
    w.server
        .send(
            peer,
            Channel::GAME_RPC,
            &DuelMsg::Chat(format!("p0-m{:05}-{}", 3, "x".repeat(100))),
        )
        .expect("queued");
    w.settle(50);
    let up: Vec<String> = (0..10)
        .map(|k| format!("p0-m{k:05}-{}", "x".repeat(100)))
        .collect();
    assert_eq!(w.players[0].up, up, "the raced chat arrived once, in order");
    assert_eq!(w.peer_addr(0), public(5, 40_000));
    assert_eq!(w.server.totals().rebinds, 0);
    w.assert_no_hijack(0);
}

/// A real NAT rebinding: the server keeps sending to the old address until the new one answers the
/// probe, then moves; nothing is lost, and the move costs one validation, not a reconnect.
#[test]
fn a_nat_rebind_moves_once_the_new_path_answers_its_probe() {
    let mut w = one_player();
    w.run_chatting(0, 5);
    w.players[0].nat.current = 1;
    w.run_chatting(5, 40);
    w.settle(80);
    assert_eq!(w.peer_addr(0), public(5, 40_001));
    w.assert_all_chats(0, 45);
    assert_eq!(w.server.totals().rebinds, 1);
    assert!(w.server.totals().path_probes >= 1);
    assert_eq!(w.row(0).rebinds, 1);
    assert!(w.left.is_empty(), "{:?}", w.left);
    w.assert_no_hijack(0);
}

/// ADR-8422 (the lockstep soak's `mobile-4g` seats): a NAT rebinding on a path whose round trip is
/// longer than the probe resend interval, while the player sends a record every step, more than
/// the per-address trial budget allows in a second. Base: the resend forgot the first probe and
/// the budget refused the records that answered it, so the server kept sending to the dead
/// address until the next second's budget (about a second without traffic). Now the move takes
/// about one round trip, no genuine record is refused, the player hears the server while the new
/// path validates (the server mirrors to it), and the port-only move keeps the round-trip
/// estimate.
#[test]
fn a_nat_rebind_on_a_long_round_trip_costs_about_one_round_trip() {
    let slow = |seed| ConditionerConfig {
        enabled: true,
        seed,
        delay_ms: 60,
        ..ConditionerConfig::default()
    };
    let mut w = World::new(ServerConfig {
        conditioner: slow(21),
        ..ServerConfig::default()
    });
    w.join(
        SocketAddr::from(([192, 168, 0, 2], 5000)),
        &[public(5, 40_000), public(5, 40_001)],
        ClientConfig {
            conditioner: slow(22),
            ..client_config("ada")
        },
    );
    let mut n = 0;
    // Chat until a second boundary, so the base's budget is spent early in its second.
    while n < 100 || !w.now.is_multiple_of(1000) {
        w.chat(0, n);
        n += 1;
        w.step();
    }
    let rtt = w.row(0).rtt_ms;
    assert!(
        rtt > PATH_PROBE_RESEND_MS as f64,
        "the round trip ({rtt} ms) is longer than the resend interval"
    );
    w.players[0].quiet = 0;
    w.players[0].longest_quiet = 0;
    let start = w.now;
    w.players[0].nat.current = 1;
    let mut moved = None;
    while w.now - start < 2_000 {
        w.chat(0, n);
        n += 1;
        w.step();
        if moved.is_none() && w.peer_addr(0) == public(5, 40_001) {
            moved = Some((w.now - start, w.row(0).rtt_ms));
        }
    }
    let (moved_after, rtt_after) = moved.expect("the peer moved");
    let longest_quiet = w.players[0].longest_quiet;
    let row = w.row(0);
    println!(
        "rtt {rtt} ms, moved after {moved_after} ms (rtt then {rtt_after}), longest quiet {longest_quiet} \
         steps, {row:?}"
    );
    // The first record from the new address arrives half a round trip after the move, the
    // probe's answer one round trip after that.
    let rtt_ms = rtt.ceil() as u64;
    assert!(
        moved_after <= rtt_ms * 3 / 2 + 4 * STEP_MS,
        "moved {moved_after} ms after the rebinding (round trip {rtt_ms} ms)"
    );
    assert_eq!(row.rebinds_refused, 0, "no genuine record refused: {row:?}");
    assert!(row.path_mirrored > 0, "{row:?}");
    assert!(
        longest_quiet * STEP_MS <= 2 * rtt_ms + 4 * STEP_MS,
        "the player heard nothing for {longest_quiet} steps (round trip {rtt_ms} ms)"
    );
    assert!(
        rtt_after > 0.0,
        "a port-only move keeps the round-trip estimate (RFC 9000 § 9.4)"
    );
    w.settle(100);
    w.assert_all_chats(0, n);
    assert_eq!(w.server.totals().rebinds, 1);
    w.assert_no_hijack(0);
}

/// A second move right after the first is not held back (base: no move for 1 s): the peer follows
/// within a round trip plus a probe interval, so a client switching back from cellular to Wi-Fi is
/// not cut off.
#[test]
fn a_move_right_after_a_move_is_not_held_back() {
    let mut w = one_player();
    w.run_chatting(0, 3);
    w.players[0].nat.current = 1;
    let mut n = 3;
    for _ in 0..20 {
        w.chat(0, n);
        n += 1;
        w.step();
        if w.peer_addr(0) == public(5, 40_001) {
            break;
        }
    }
    assert_eq!(w.peer_addr(0), public(5, 40_001), "first move");
    let moved_at = w.now;
    w.players[0].nat.current = 2;
    for _ in 0..40 {
        w.chat(0, n);
        n += 1;
        w.step();
        if w.peer_addr(0) == public(9, 50_000) {
            break;
        }
    }
    assert_eq!(w.peer_addr(0), public(9, 50_000), "second move");
    assert!(
        w.now - moved_at <= 2 * STEP_MS + PATH_PROBE_RESEND_MS + 5 * STEP_MS,
        "the second move took {} ms",
        w.now - moved_at
    );
    w.settle(80);
    w.assert_all_chats(0, n);
    assert_eq!(w.server.totals().rebinds, 2);
}

/// Adversarial row 12: `REBIND_ATTEMPTS_PER_S` forged records from elsewhere in one second do not
/// spend the trial budget of the address the player really moved to: the genuine move completes in
/// the same second, and the forgeries are counted on the peer's row.
#[test]
fn forgeries_from_elsewhere_leave_the_real_new_address_its_budget() {
    let mut w = one_player();
    w.run_chatting(0, 3);
    w.settle(20);
    // Start on a second boundary, so "the same second" is the next 1000 ms.
    while !w.now.is_multiple_of(1000) {
        w.step();
    }
    let genuine = w.players[0]
        .nat
        .upstream
        .iter()
        .rev()
        .find(|d| split_datagram(d).is_some_and(|(k, _)| k == PacketKind::Sealed))
        .cloned()
        .expect("a sealed datagram");
    let mut attacker = w.net.bind(sa(ATTACKER)).expect("bind");
    let auth_before = w.server.totals().auth_failed;
    for i in 0..u64::from(REBIND_ATTEMPTS_PER_S) {
        let mut f = genuine.clone();
        let last = f.len() - 1;
        f[last] ^= 1;
        f[SEALED_HEADER_BYTES - 8..SEALED_HEADER_BYTES].copy_from_slice(&(1_000_000 + i).to_le_bytes());
        attacker.send_to(&f, sa(SERVER)).expect("send");
    }
    w.server.receive(w.now);
    assert_eq!(
        w.server.totals().auth_failed - auth_before,
        u64::from(REBIND_ATTEMPTS_PER_S),
        "the forgeries reached the cipher (their address's budget)"
    );
    let start = w.now;
    w.players[0].nat.current = 1;
    let mut n = 3;
    while w.now - start < 500 && w.peer_addr(0) != public(5, 40_001) {
        w.chat(0, n);
        n += 1;
        w.step();
    }
    assert_eq!(
        w.peer_addr(0),
        public(5, 40_001),
        "the genuine move completed in the second of the forgeries"
    );
    let row = w.row(0);
    assert_eq!(
        row.rebinds_refused,
        u64::from(REBIND_ATTEMPTS_PER_S),
        "refusals on the peer row: {row:?}"
    );
    w.settle(80);
    w.assert_all_chats(0, n);
}

/// CGNAT (acceptance): eight players share one carrier-grade NAT address (the per-address cap);
/// a ninth joins over Wi-Fi and its phone then moves to the same carrier NAT. It is the same
/// player with its one slot, so the move is allowed and it stays connected.
#[test]
fn a_ninth_player_moving_into_a_full_cgnat_block_stays_connected() {
    let mut w = World::new(ServerConfig::default());
    let cgnat = |port: u16| SocketAddr::from(([100, 64, 0, 1], port));
    for i in 0..8u16 {
        w.join(
            SocketAddr::from(([10, 20, 0, 2 + i as u8], 5000)),
            &[cgnat(30_000 + i)],
            client_config(&format!("p{i}")),
        );
    }
    let ninth = w.join(
        SocketAddr::from(([192, 168, 1, 50], 5000)),
        &[public(77, 41_000), cgnat(30_008)],
        client_config("roamer"),
    );
    assert_eq!(w.server.session().len(), 9);
    w.run_chatting(0, 10);
    w.players[ninth].nat.current = 1;
    // Thirty seconds of play, past the 20 s silence timeout.
    w.run_chatting(10, 3_000);
    w.settle(100);
    assert_eq!(w.peer_addr(ninth), cgnat(30_008));
    assert!(w.left.is_empty(), "nobody left: {:?}", w.left);
    assert_eq!(w.players[ninth].ended, None);
    assert_eq!(w.server.session().len(), 9);
    for i in 0..9 {
        w.assert_all_chats(i, 3_010);
        w.assert_no_hijack(i);
    }
}

/// nat-rebind (acceptance): the player's NAT gives it a new public address every 30 s for ten
/// minutes on bad Wi-Fi. No disconnect, no reliable message lost, every move validated.
#[test]
fn nat_rebind_every_30_s_for_10_min_on_bad_wifi_drops_nothing() {
    let outside: Vec<SocketAddr> = (0..21u16).map(|i| public(5, 40_000 + i)).collect();
    let mut w = World::new(ServerConfig {
        conditioner: bad_wifi(11),
        ..ServerConfig::default()
    });
    let p = w.join(
        SocketAddr::from(([192, 168, 0, 2], 5000)),
        &outside,
        ClientConfig {
            conditioner: bad_wifi(12),
            ..client_config("ada")
        },
    );
    let minute_steps = (60_000 / STEP_MS) as usize;
    let rebind_every = (30_000 / STEP_MS) as usize;
    let mut sent = 0;
    // Moves at 30 s, 60 s, ..., 600 s: twenty.
    for s in 0..=10 * minute_steps {
        if s > 0 && s.is_multiple_of(rebind_every) {
            w.players[p].nat.current += 1;
        }
        // One reliable chat each way every 100 ms.
        if s.is_multiple_of(10) {
            w.chat(p, sent);
            sent += 1;
        }
        w.step();
    }
    w.settle(500);
    assert_eq!(w.players[p].ended, None, "no disconnect");
    assert!(w.left.is_empty(), "no disconnect: {:?}", w.left);
    w.assert_all_chats(p, sent);
    assert_eq!(w.peer_addr(p), outside[20]);
    assert_eq!(w.server.totals().rebinds, 20, "{:?}", w.server.totals());
    w.assert_no_hijack(p);
}

/// no_hijack under hostile forwarding and a NAT rebind together: an on-path observer races every
/// record while the player's NAT moves it. The genuine copies, arriving as duplicates from the new
/// address, still show the player there; the peer moves to its real address, never to the
/// observer's, and nothing is lost.
#[test]
fn no_hijack_when_a_racer_copies_every_record_across_a_nat_rebind() {
    let mut w = one_player();
    w.players[0].nat.racer = Some(Racer {
        socket: w.net.bind(sa(ATTACKER)).expect("bind"),
        sent: 0,
        received: 0,
    });
    w.run_chatting(0, 50);
    w.players[0].nat.current = 1;
    w.run_chatting(50, 200);
    w.settle(100);
    assert_eq!(w.peer_addr(0), public(5, 40_001), "moved to the real new address");
    assert_eq!(w.server.totals().rebinds, 1);
    w.assert_all_chats(0, 250);
    w.assert_no_hijack(0);
    let racer = w.players[0].nat.racer.as_ref().expect("racer");
    assert!(racer.received <= ANTI_AMPLIFICATION_FACTOR * racer.sent);
}

/// Plain links (no encryption) migrate the same way: the token and a newer packet sequence make
/// a candidate, and the move waits for the probe's acknowledgment from the new address.
#[test]
fn a_plain_link_follows_a_nat_rebind_after_the_probe() {
    let mut w = World::new(ServerConfig {
        crypto: CryptoMode::Off,
        ..ServerConfig::default()
    });
    w.join(
        SocketAddr::from(([192, 168, 0, 2], 5000)),
        &[public(5, 40_000), public(5, 40_001)],
        ClientConfig {
            crypto: CryptoMode::Off,
            ..client_config("ada")
        },
    );
    assert!(!w.players[0].client.is_sealed());
    w.run_chatting(0, 5);
    w.players[0].nat.current = 1;
    w.run_chatting(5, 40);
    w.settle(80);
    assert_eq!(w.peer_addr(0), public(5, 40_001));
    w.assert_all_chats(0, 45);
    assert_eq!(w.server.totals().rebinds, 1);
    assert!(w.server.totals().path_probes >= 1);
}

/// The per-datagram address lookup costs the same at 8 peers as at 128: one map probe per datagram
/// (base: a scan of every peer).
#[test]
fn per_datagram_lookups_stay_flat_from_8_to_128_peers() {
    fn probes_per_datagram(peers: usize) -> f64 {
        let mut w = World::new(ServerConfig {
            max_peers: 160,
            max_peers_per_ip: 0,
            ..ServerConfig::default()
        });
        for i in 0..peers {
            let i = i as u16;
            w.join(
                SocketAddr::from(([192, 168, (i >> 8) as u8, (i & 0xff) as u8], 5000)),
                &[SocketAddr::from((
                    [203, 0, (i >> 8) as u8 + 1, (i & 0xff) as u8],
                    40_000,
                ))],
                client_config(&format!("p{i}")),
            );
        }
        w.settle(20);
        let (probes, datagrams) = (w.server.session().addr_probes(), w.server.totals().datagrams_in);
        w.run_chatting(0, 50);
        let probes = w.server.session().addr_probes() - probes;
        let datagrams = w.server.totals().datagrams_in - datagrams;
        assert!(datagrams >= 50 * peers as u64, "{datagrams} datagrams");
        probes as f64 / datagrams as f64
    }
    let small = probes_per_datagram(8);
    let large = probes_per_datagram(128);
    assert!(small <= 1.0 + 1e-9, "{small} probes per datagram at 8 peers");
    assert!(
        (small - large).abs() < 1e-9,
        "8 peers: {small}, 128 peers: {large}"
    );
}

// --- Client side (adversarial row 13) -------------------------------------------------------------

/// A server-side NAT: the server hears every client datagram from `inside`; clients hear the
/// server from `outside[current]`. Any outside address forwards to the server.
struct ServerNat {
    inside: LoopbackSocket,
    outside: Vec<LoopbackSocket>,
    current: usize,
    client: SocketAddr,
    server: SocketAddr,
    racer: Option<Racer>,
}

impl ServerNat {
    fn pump(&mut self) {
        let mut buf = [0u8; 2048];
        for socket in &mut self.outside {
            while let Some((n, _)) = socket.recv_from(&mut buf).expect("io") {
                self.inside.send_to(&buf[..n], self.server).expect("send");
            }
        }
        while let Some((n, _)) = self.inside.recv_from(&mut buf).expect("io") {
            if let Some(r) = self.racer.as_mut() {
                r.socket.send_to(&buf[..n], self.client).expect("send");
                r.sent += n as u64;
            }
            self.outside[self.current]
                .send_to(&buf[..n], self.client)
                .expect("send");
        }
        if let Some(r) = self.racer.as_mut() {
            r.drain();
        }
    }
}

struct ServerSide {
    _net: LoopbackNet,
    server: NetServer<Duel, LoopbackSocket>,
    client: NetClient<Duel, LoopbackSocket>,
    nat: ServerNat,
    now: u64,
    up: Vec<String>,
    down: Vec<String>,
    ended: Option<ClientEnd>,
}

const SERVER_PUBLIC: [([u8; 4], u16); 2] = [([203, 0, 113, 80], 7777), ([203, 0, 113, 80], 61_234)];

fn server_side(racer: bool) -> ServerSide {
    server_side_with(racer, ServerConfig::default().crypto)
}

/// [`server_side`] with both ends on `crypto` (sealed unless it is [`CryptoMode::Off`]).
fn server_side_with(racer: bool, crypto: CryptoMode) -> ServerSide {
    let net = LoopbackNet::new();
    let server = NetServer::with_secret(
        net.bind(sa(SERVER)).expect("bind"),
        ServerConfig {
            crypto,
            ..ServerConfig::default()
        },
        [7; 32],
    );
    let client_addr = SocketAddr::from(([192, 168, 0, 2], 5000));
    let client = NetClient::connect(
        net.bind(client_addr).expect("bind"),
        sa(SERVER_PUBLIC[0]),
        ClientConfig {
            crypto,
            ..client_config("ada")
        },
        &Team(1),
    )
    .expect("join fits");
    let nat = ServerNat {
        inside: net.bind(SocketAddr::from(([10, 0, 0, 254], 9000))).expect("bind"),
        outside: SERVER_PUBLIC
            .iter()
            .map(|a| net.bind(sa(*a)).expect("bind"))
            .collect(),
        current: 0,
        client: client_addr,
        server: sa(SERVER),
        racer: racer.then(|| Racer {
            socket: net.bind(sa(ATTACKER)).expect("bind"),
            sent: 0,
            received: 0,
        }),
    };
    let mut r = ServerSide {
        _net: net,
        server,
        client,
        nat,
        now: 0,
        up: Vec::new(),
        down: Vec::new(),
        ended: None,
    };
    for _ in 0..300 {
        if r.client.is_connected() {
            break;
        }
        r.step();
    }
    assert!(r.client.is_connected());
    assert_eq!(r.client.is_sealed(), crypto != CryptoMode::Off);
    r
}

impl ServerSide {
    fn step(&mut self) {
        self.now += STEP_MS;
        self.client.update(self.now);
        self.nat.pump();
        self.server.update(self.now);
        self.nat.pump();
        while let Some(e) = self.server.poll_event() {
            if let ServerEvent::Message {
                message: DuelMsg::Chat(t),
                ..
            } = e
            {
                self.up.push(t);
            }
        }
        while let Some(e) = self.client.poll_event() {
            match e {
                ClientEvent::Message {
                    message: DuelMsg::Chat(t),
                    ..
                } => self.down.push(t),
                ClientEvent::Ended(why) => self.ended = Some(why),
                _ => {}
            }
        }
    }

    fn run_chatting(&mut self, first: usize, steps: usize) {
        for n in first..first + steps {
            let text = format!("m{n:05}");
            self.client
                .send(Channel::GAME_RPC, &DuelMsg::Chat(text.clone()))
                .expect("queued");
            let peer = self.server.session().peers().next().expect("peer").id;
            self.server
                .send(peer, Channel::GAME_RPC, &DuelMsg::Chat(text))
                .expect("queued");
            self.step();
        }
    }

    fn remote(&self) -> String {
        self.client.status().remote_addr.expect("remote")
    }
}

/// Row 13: the server's source port changes (its NAT moved it). The client validates the new
/// address and follows it; traffic and tick syncs keep flowing and it stays connected past the
/// silence timeout.
#[test]
fn the_client_follows_a_validated_server_address_change() {
    let mut r = server_side(false);
    r.run_chatting(0, 5);
    r.nat.current = 1;
    r.run_chatting(5, 2_500);
    for _ in 0..100 {
        r.step();
    }
    assert_eq!(r.ended, None, "still connected");
    assert_eq!(r.remote(), sa(SERVER_PUBLIC[1]).to_string());
    assert_eq!(r.client.totals().rebinds, 1);
    let expected: Vec<String> = (0..2_505).map(|n| format!("m{n:05}")).collect();
    assert_eq!(r.up, expected);
    assert_eq!(r.down, expected);
}

/// ADR-8422, client side: when the server's NAT moves its port, the client refuses none of the
/// server's records from the new address, copies its own traffic there while the old address is
/// silent (within three times what the new address sent), and follows on the probe's answer.
#[test]
fn the_client_copies_to_a_moved_server_while_the_old_address_is_silent() {
    let mut r = server_side(false);
    r.run_chatting(0, 5);
    r.nat.current = 1;
    r.run_chatting(5, 200);
    for _ in 0..100 {
        r.step();
    }
    assert_eq!(r.ended, None, "still connected");
    assert_eq!(r.remote(), sa(SERVER_PUBLIC[1]).to_string());
    let totals = r.client.totals();
    assert_eq!(totals.rebinds, 1, "{totals:?}");
    assert_eq!(totals.rebinds_refused, 0, "{totals:?}");
    assert!(totals.path_mirrored > 0, "{totals:?}");
    let expected: Vec<String> = (0..205).map(|n| format!("m{n:05}")).collect();
    assert_eq!(r.up, expected);
    assert_eq!(r.down, expected);
}

/// Row 13, hostile: a copy of every server record raced to the client from a third address does
/// not move the client, and the third address gets at most three times the bytes it sent.
#[test]
fn a_raced_server_record_does_not_move_the_client() {
    let mut r = server_side(true);
    r.run_chatting(0, 300);
    for _ in 0..100 {
        r.step();
    }
    assert_eq!(r.ended, None);
    assert_eq!(r.remote(), sa(SERVER_PUBLIC[0]).to_string(), "the client stayed");
    assert_eq!(r.client.totals().rebinds, 0);
    let expected: Vec<String> = (0..300).map(|n| format!("m{n:05}")).collect();
    assert_eq!(r.up, expected);
    assert_eq!(r.down, expected);
    let racer = r.nat.racer.as_ref().expect("racer");
    assert!(racer.received <= ANTI_AMPLIFICATION_FACTOR * racer.sent);
    assert!(r.client.totals().path_probes > 0);
}

/// KI-NT-018 (ADR-10077), server side: on a plain link nothing authenticates a packet, so one
/// from an address the peer never proved it holds is not applied until a packet from there
/// answers the probe. The client's datagram carrying a reliable chat is taken off its path and
/// sent from a third address only: the server applies none of it (base: the chat was delivered
/// from the unvalidated address), the peer stays, and the chat still arrives exactly once, from
/// the validated path's resend.
#[test]
fn a_plain_packet_from_an_unvalidated_address_is_not_applied() {
    let mut w = World::new(ServerConfig {
        crypto: CryptoMode::Off,
        ..ServerConfig::default()
    });
    w.join(
        SocketAddr::from(([192, 168, 0, 2], 5000)),
        &[public(5, 40_000)],
        ClientConfig {
            crypto: CryptoMode::Off,
            ..client_config("ada")
        },
    );
    assert!(!w.players[0].client.is_sealed());
    w.run_chatting(0, 5);
    w.settle(50);
    let diverted_text = "diverted".to_string();
    w.players[0]
        .client
        .send(Channel::GAME_RPC, &DuelMsg::Chat(diverted_text.clone()))
        .expect("queued");
    w.now += STEP_MS;
    w.players[0].client.update(w.now);
    let mut diverted = Vec::new();
    let mut buf = [0u8; 2048];
    while let Some((n, _)) = w.players[0].nat.inside.recv_from(&mut buf).expect("io") {
        diverted.push(buf[..n].to_vec());
    }
    assert!(!diverted.is_empty(), "the client sent the chat");
    let mut attacker = w.net.bind(sa(ATTACKER)).expect("bind");
    for d in &diverted {
        attacker.send_to(d, sa(SERVER)).expect("send");
    }
    w.server.update(w.now);
    while let Some(e) = w.server.poll_event() {
        if let ServerEvent::Message {
            message: DuelMsg::Chat(text),
            ..
        } = e
        {
            w.players[0].up.push(text);
        }
    }
    assert!(
        !w.players[0].up.contains(&diverted_text),
        "a plain packet from an unvalidated address was applied"
    );
    assert_eq!(w.peer_addr(0), public(5, 40_000));
    w.settle(300);
    assert_eq!(
        w.players[0].up.iter().filter(|t| **t == diverted_text).count(),
        1,
        "the chat arrives once, by the validated path's resend"
    );
    assert_eq!(w.peer_addr(0), public(5, 40_000));
    assert_eq!(w.server.totals().rebinds, 0);
    assert_eq!(w.players[0].ended, None);
}

/// KI-NT-018 (ADR-10077), client side: the same for the server's datagrams. A plain server
/// packet diverted to the client from a third address is not applied before that address answers
/// a probe; the chat arrives once, by the validated path, and the client stays on its server.
#[test]
fn a_plain_server_packet_from_an_unvalidated_address_is_not_applied() {
    let mut r = server_side_with(false, CryptoMode::Off);
    r.run_chatting(0, 5);
    for _ in 0..50 {
        r.step();
    }
    let peer = r.server.session().peers().next().expect("peer").id;
    let diverted_text = "diverted".to_string();
    r.server
        .send(peer, Channel::GAME_RPC, &DuelMsg::Chat(diverted_text.clone()))
        .expect("queued");
    r.now += STEP_MS;
    r.server.update(r.now);
    let mut diverted = Vec::new();
    let mut buf = [0u8; 2048];
    while let Some((n, _)) = r.nat.inside.recv_from(&mut buf).expect("io") {
        diverted.push(buf[..n].to_vec());
    }
    assert!(!diverted.is_empty(), "the server sent the chat");
    let mut attacker = r._net.bind(sa(ATTACKER)).expect("bind");
    for d in &diverted {
        attacker.send_to(d, r.nat.client).expect("send");
    }
    r.client.update(r.now);
    while let Some(e) = r.client.poll_event() {
        if let ClientEvent::Message {
            message: DuelMsg::Chat(t),
            ..
        } = e
        {
            r.down.push(t);
        }
    }
    assert!(
        !r.down.contains(&diverted_text),
        "a plain server packet from an unvalidated address was applied"
    );
    for _ in 0..300 {
        r.step();
    }
    assert_eq!(r.down.iter().filter(|t| **t == diverted_text).count(), 1);
    assert_eq!(r.ended, None);
    assert_eq!(r.remote(), sa(SERVER_PUBLIC[0]).to_string(), "the client stayed");
    assert_eq!(r.client.totals().rebinds, 0);
}
