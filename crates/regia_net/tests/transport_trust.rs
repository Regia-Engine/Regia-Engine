// SPDX-License-Identifier: MPL-2.0
//! Session rekeying and NAT rebinding end to end (ADR-2416), over the in-memory network.
//!
//! A [`Nat`] stands between client and server the way a home router does: the server sees the
//! NAT's public address, never the client's, and the NAT can switch to another public address
//! mid-session (a rebinding). Time is a step counter; nothing sleeps.

mod common;

use std::net::SocketAddr;

use common::{Duel, DuelMsg, Team};
use regia_net::LoopbackSocket;
use regia_net::crypto::{RekeyPolicy, SEALED_HEADER_BYTES};
use regia_net::handshake::{PacketKind, split_datagram};
use regia_net::prelude::*;
use regia_net::server::{REBIND_ATTEMPTS_PER_S, REBIND_MIN_INTERVAL_MS};

const STEP_MS: u64 = 10;
const SERVER: ([u8; 4], u16) = ([10, 0, 0, 1], 7777);
const CLIENT: ([u8; 4], u16) = ([192, 168, 0, 2], 5000);
const NAT_INSIDE: ([u8; 4], u16) = ([192, 168, 0, 1], 7777);
const PUBLIC_A: ([u8; 4], u16) = ([203, 0, 113, 5], 40_000);
const PUBLIC_B: ([u8; 4], u16) = ([203, 0, 113, 5], 40_001);
const PUBLIC_C: ([u8; 4], u16) = ([198, 51, 100, 9], 50_000);
const ATTACKER: ([u8; 4], u16) = ([198, 51, 100, 66], 6666);

/// A NAT: the client talks to `inside`; the server hears `outside[current]`.
struct Nat {
    inside: LoopbackSocket,
    outside: Vec<LoopbackSocket>,
    current: usize,
    client: SocketAddr,
    server: SocketAddr,
    /// Every client-to-server datagram, as the server would see it.
    upstream: Vec<Vec<u8>>,
}

impl Nat {
    fn pump(&mut self) {
        let mut buf = [0u8; 2048];
        while let Some((n, _)) = self.inside.recv_from(&mut buf).expect("io") {
            self.upstream.push(buf[..n].to_vec());
            self.outside[self.current]
                .send_to(&buf[..n], self.server)
                .expect("send");
        }
        for socket in &mut self.outside {
            while let Some((n, _)) = socket.recv_from(&mut buf).expect("io") {
                self.inside.send_to(&buf[..n], self.client).expect("send");
            }
        }
    }
}

struct Rig {
    /// Keeps the in-memory network alive.
    _net: LoopbackNet,
    server: NetServer<Duel, LoopbackSocket>,
    client: NetClient<Duel, LoopbackSocket>,
    nat: Nat,
    /// Sends from outside the NAT without any key.
    attacker: LoopbackSocket,
    now: u64,
}

fn rig(rekey: RekeyPolicy) -> Rig {
    let net = LoopbackNet::new();
    let server_config = ServerConfig {
        rekey,
        ..ServerConfig::default()
    };
    let server = NetServer::with_secret(
        net.bind(SocketAddr::from(SERVER)).expect("bind"),
        server_config,
        [7; 32],
    );
    let client_config = ClientConfig {
        name: "ada".into(),
        rekey,
        ..ClientConfig::default()
    };
    let client = NetClient::connect(
        net.bind(SocketAddr::from(CLIENT)).expect("bind"),
        SocketAddr::from(NAT_INSIDE),
        client_config,
        &Team(1),
    )
    .expect("join fits");
    let nat = Nat {
        inside: net.bind(SocketAddr::from(NAT_INSIDE)).expect("bind"),
        outside: [PUBLIC_A, PUBLIC_B, PUBLIC_C]
            .into_iter()
            .map(|a| net.bind(SocketAddr::from(a)).expect("bind"))
            .collect(),
        current: 0,
        client: SocketAddr::from(CLIENT),
        server: SocketAddr::from(SERVER),
        upstream: Vec::new(),
    };
    let attacker = net.bind(SocketAddr::from(ATTACKER)).expect("bind");
    let mut rig = Rig {
        _net: net,
        server,
        client,
        nat,
        attacker,
        now: 0,
    };
    for _ in 0..200 {
        if rig.client.is_connected() {
            break;
        }
        rig.step();
    }
    assert!(rig.client.is_connected(), "{:?}", rig.client.state());
    assert!(rig.client.is_sealed());
    rig.drain_server();
    rig
}

impl Rig {
    fn step(&mut self) {
        self.now += STEP_MS;
        self.client.update(self.now);
        self.nat.pump();
        self.server.update(self.now);
        self.nat.pump();
    }

    fn peer(&self) -> PeerId {
        self.server.session().peers().next().expect("a peer").id
    }

    fn peer_addr(&self) -> SocketAddr {
        self.server.session().peers().next().expect("a peer").addr
    }

    fn drain_server(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        while let Some(e) = self.server.poll_event() {
            if let ServerEvent::Message {
                message: DuelMsg::Chat(text),
                ..
            } = e
            {
                out.push(text);
            }
        }
        out
    }

    fn drain_client(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        while let Some(e) = self.client.poll_event() {
            if let ClientEvent::Message {
                message: DuelMsg::Chat(text),
                ..
            } = e
            {
                out.push(text);
            }
        }
        out
    }

    /// Sends `n` chats each way, numbered from `first`, one per step; returns what arrived.
    fn exchange(&mut self, first: usize, n: usize) -> (Vec<String>, Vec<String>) {
        let (mut up, mut down) = (Vec::new(), Vec::new());
        for i in first..first + n {
            let text = format!("m{i:05}-{}", "x".repeat(200));
            self.client
                .send(Channel::GAME_RPC, &DuelMsg::Chat(text.clone()))
                .expect("queued");
            let peer = self.peer();
            self.server
                .send(peer, Channel::GAME_RPC, &DuelMsg::Chat(text))
                .expect("queued");
            self.step();
            up.extend(self.drain_server());
            down.extend(self.drain_client());
        }
        for _ in 0..50 {
            self.step();
            up.extend(self.drain_server());
            down.extend(self.drain_client());
        }
        (up, down)
    }

    /// A datagram from the attacker to the server; the server reads it at once.
    fn inject(&mut self, bytes: &[u8]) {
        self.attacker
            .send_to(bytes, SocketAddr::from(SERVER))
            .expect("send");
        self.server.receive(self.now);
    }

    /// The last sealed datagram the client sent.
    fn last_sealed_upstream(&self) -> Vec<u8> {
        self.nat
            .upstream
            .iter()
            .rev()
            .find(|d| split_datagram(d).is_some_and(|(k, _)| k == PacketKind::Sealed))
            .cloned()
            .expect("a sealed datagram")
    }
}

fn expected(first: usize, n: usize) -> Vec<String> {
    (first..first + n)
        .map(|i| format!("m{i:05}-{}", "x".repeat(200)))
        .collect()
}

/// Both directions switch keys at a byte count, several times, with every message delivered once
/// and in order and no record refused as a replay or a forgery.
#[test]
fn a_rekey_at_a_byte_count_loses_and_replays_no_packet() {
    let mut r = rig(RekeyPolicy {
        after_records: 0,
        after_bytes: 32 * 1024,
    });
    let (up, down) = r.exchange(0, 600);
    // The server's last flush is still on its way; let the client read it.
    r.client.receive(r.now);
    assert_eq!(up, expected(0, 600), "client to server");
    assert_eq!(down, expected(0, 600), "server to client");
    let server = r.server.status();
    let client = r.client.status();
    assert!(
        client.crypto.rekeys >= 4,
        "client rekeys: {}",
        client.crypto.rekeys
    );
    assert_eq!(
        client.crypto.rekeys, server.crypto.rekeys,
        "both sides on the same keys"
    );
    for totals in [server.totals, client.totals] {
        assert_eq!(totals.replayed, 0, "{totals:?}");
        assert_eq!(totals.auth_failed, 0, "{totals:?}");
    }
}

/// The NAT moves the client to a new public address mid-session: the server follows on the first
/// authenticated record, replies go to the new address, and the session goes on.
#[test]
fn a_rebind_is_accepted_with_the_key() {
    let mut r = rig(RekeyPolicy::default());
    assert_eq!(r.peer_addr(), SocketAddr::from(PUBLIC_A));
    let (up, _) = r.exchange(0, 5);
    assert_eq!(up, expected(0, 5));
    r.nat.current = 1;
    let (up, down) = r.exchange(5, 20);
    assert_eq!(up, expected(5, 20), "nothing lost across the move");
    assert_eq!(down, expected(5, 20), "replies reach the new address");
    assert_eq!(r.peer_addr(), SocketAddr::from(PUBLIC_B));
    let totals = r.server.totals();
    assert_eq!(totals.rebinds, 1, "{totals:?}");
    assert_eq!(r.server.session().len(), 1, "the same peer, not a new join");
}

/// Records that carry the peer's token from another address but not its key are refused, and so
/// is a captured genuine record replayed from there: the peer stays where it is.
#[test]
fn a_rebind_is_refused_without_the_key() {
    let mut r = rig(RekeyPolicy::default());
    r.exchange(0, 3);
    let genuine = r.last_sealed_upstream();
    let mut forged = genuine.clone();
    let last = forged.len() - 1;
    forged[last] ^= 1;
    r.inject(&forged);
    assert_eq!(r.peer_addr(), SocketAddr::from(PUBLIC_A));
    // The genuine record again, from another address: already seen, so not the newest.
    r.inject(&genuine);
    assert_eq!(r.peer_addr(), SocketAddr::from(PUBLIC_A));
    // A record with a far newer counter but no key: still the tag decides.
    let mut ahead = genuine.clone();
    ahead[SEALED_HEADER_BYTES - 8..SEALED_HEADER_BYTES].copy_from_slice(&1_000_000u64.to_le_bytes());
    r.inject(&ahead);
    assert_eq!(r.peer_addr(), SocketAddr::from(PUBLIC_A));
    let totals = r.server.totals();
    assert_eq!(totals.rebinds, 0, "{totals:?}");
    assert_eq!(totals.rebinds_refused, 3, "{totals:?}");
    // The real client is unaffected.
    let (up, down) = r.exchange(3, 5);
    assert_eq!(up, expected(3, 5));
    assert_eq!(down, expected(3, 5));
}

/// Forged rebinds cost at most the per-second budget of trial decryptions of their address. A move
/// right after a move is not held back any more (ADR-8299 replaced the one-second interval with path
/// validation: a move needs the new path to answer a probe, so copied records cannot make the
/// traffic flap); the records sent meanwhile all arrive.
#[test]
fn rebinds_are_rate_limited() {
    let mut r = rig(RekeyPolicy::default());
    r.exchange(0, 3);
    let mut forged = r.last_sealed_upstream();
    let last = forged.len() - 1;
    forged[last] ^= 1;
    let auth_before = r.server.totals().auth_failed;
    // One step of server time: far more forgeries than the budget, each with a new counter.
    let tries = REBIND_ATTEMPTS_PER_S as u64 * 4;
    for i in 0..tries {
        let mut f = forged.clone();
        f[SEALED_HEADER_BYTES - 8..SEALED_HEADER_BYTES].copy_from_slice(&(10_000 + i).to_le_bytes());
        r.attacker.send_to(&f, SocketAddr::from(SERVER)).expect("send");
    }
    r.server.receive(r.now);
    let totals = r.server.totals();
    assert_eq!(
        totals.auth_failed - auth_before,
        u64::from(REBIND_ATTEMPTS_PER_S),
        "only the budget reached the cipher"
    );
    assert_eq!(totals.rebinds_refused, tries);
    // Wait out the second, then move once: accepted.
    for _ in 0..(1000 / STEP_MS) {
        r.step();
    }
    r.nat.current = 1;
    let (up, _) = r.exchange(3, 3);
    assert_eq!(up, expected(3, 3));
    assert_eq!(r.peer_addr(), SocketAddr::from(PUBLIC_B));
    // A second move right after it: followed once the new path answers its probe, well inside the
    // old interval.
    r.nat.current = 2;
    let steps = (REBIND_MIN_INTERVAL_MS / STEP_MS) as usize + 50;
    let mut up = Vec::new();
    let mut moved_at = None;
    for i in 0..steps {
        if i < 3 {
            let text = format!("m{:05}-{}", 6 + i, "x".repeat(200));
            r.client
                .send(Channel::GAME_RPC, &DuelMsg::Chat(text))
                .expect("queued");
        }
        r.step();
        up.extend(r.drain_server());
        if moved_at.is_none() && r.peer_addr() == SocketAddr::from(PUBLIC_C) {
            moved_at = Some(i);
        }
    }
    assert!(
        moved_at.is_some_and(|i| i < 20),
        "the second move is not held for the old interval: {moved_at:?}"
    );
    assert_eq!(r.peer_addr(), SocketAddr::from(PUBLIC_C));
    assert_eq!(up, expected(6, 3));
    assert_eq!(r.server.totals().rebinds, 2);
}
