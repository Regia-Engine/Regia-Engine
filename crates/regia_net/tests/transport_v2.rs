// SPDX-License-Identifier: MPL-2.0
//! Transport v2 acceptance (spec 120 § 4.1, § 4.5 A6/A7, plan row N201, ADR-8184): packet
//! numbers, acknowledgment ranges and RFC 9002 loss detection, measured on the public
//! `Connection` API over the crate's seeded conditioner (virtual time, 1 ms steps, no sleeps).
//!
//! Every test here uses only API that the wire-4 transport had too, so the same file run on the
//! base commit gives the "before" numbers printed by each test.
use std::net::SocketAddr;

use regia_net::conditioner::{ConditionerConfig, NetConditioner};
use regia_net::connection::{Channel, Connection, LinkConfig};
use regia_net::handshake::{ConnectRequest, Deny, PacketKind, split_datagram};
use regia_net::prelude::*;
use regia_net::version_probe::{MismatchSide, ProbeAnswer, ProbeRequest, VersionStamp};
use regia_net::wire::{WireError, WireReader, WireWriter, fnv1a_64};

/// The default link with the first flight named off (ADR-10156; off by default in v0.7,
/// ADR-10231, and a later release may turn it on): these
/// tests measure the transport's blind start (the 64-packet initial window and what it makes of
/// acknowledgments, bursts and rates), which the first flight replaces until a path answers.
fn blind_link() -> LinkConfig {
    let mut config = LinkConfig::default();
    config.rate.first_flight = false;
    config
}

fn addr() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 1))
}

fn stamp(kind: u8, index: u32, now: u64, len: usize) -> Vec<u8> {
    let mut m = vec![0u8; len.max(13)];
    m[0] = kind;
    m[1..5].copy_from_slice(&index.to_le_bytes());
    m[5..13].copy_from_slice(&now.to_le_bytes());
    m
}

fn sent_at(m: &[u8]) -> u64 {
    let mut at = [0u8; 8];
    at.copy_from_slice(&m[5..13]);
    u64::from_le_bytes(at)
}

fn percentile(values: &mut [u64], p: usize) -> u64 {
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    values[(values.len() - 1) * p / 100]
}

/// Two connections over two seeded conditioners, stepped 1 ms at a time.
struct Pair {
    a: Connection,
    b: Connection,
    up: NetConditioner,
    down: NetConditioner,
    /// Datagrams each direction carried to the receiver (duplicates included).
    up_delivered: u64,
    down_delivered: u64,
    /// When set, both directions drop everything (an outage).
    cut: bool,
}

impl Pair {
    fn new(link: ConditionerConfig, seed: u64) -> Self {
        Self {
            a: Connection::new(LinkConfig::default()),
            b: Connection::new(LinkConfig::default()),
            up: NetConditioner::new(ConditionerConfig { seed, ..link.clone() }),
            down: NetConditioner::new(ConditionerConfig {
                seed: seed + 1,
                ..link
            }),
            up_delivered: 0,
            down_delivered: 0,
            cut: false,
        }
    }

    fn step(&mut self, now: u64) {
        for p in self.a.poll(now, &[]) {
            if self.cut {
                continue;
            }
            for (p, _) in self.up.process(p, addr(), now) {
                self.up_delivered += 1;
                self.b.receive(&p, now).expect("valid packet");
            }
        }
        for (p, _) in self.up.release_due(now) {
            self.up_delivered += 1;
            self.b.receive(&p, now).expect("valid packet");
        }
        for p in self.b.poll(now, &[]) {
            if self.cut {
                continue;
            }
            for (p, _) in self.down.process(p, addr(), now) {
                self.down_delivered += 1;
                self.a.receive(&p, now).expect("valid packet");
            }
        }
        for (p, _) in self.down.release_due(now) {
            self.down_delivered += 1;
            self.a.receive(&p, now).expect("valid packet");
        }
    }
}

fn link(delay_ms: u64, jitter_ms: u64, loss: f64, reorder: f64, reorder_max_ms: u64) -> ConditionerConfig {
    ConditionerConfig {
        enabled: true,
        delay_ms,
        jitter_ms,
        loss,
        reorder,
        reorder_max_ms,
        ..Default::default()
    }
}

/// Audit experiment E2 rerun (spec 120 § 15.2 row T5): 5 % loss each way, 40 ms one-way, a 60 Hz
/// unreliable stream each way and one small reliable message every 100 ms each way. The small
/// reliable p95 must stay within 1.5x the one-way delay on every seed (audit: 140 ms, 3.5x; the
/// wire-4 base of this lane: 110, 40 and 124 ms on the three seeds).
#[test]
fn e2_small_reliable_p95_within_one_and_a_half_one_way_delays_at_5_percent_loss() {
    const ONE_WAY: u64 = 40;
    let mut latencies = Vec::new();
    let mut worst = 0;
    for seed in [101u64, 202, 303] {
        let mut pair = Pair::new(link(ONE_WAY, 0, 0.05, 0.0, 0), seed);
        let mut lat = Vec::new();
        let mut index = 0u32;
        for now in 0..62_000u64 {
            if now < 60_000 {
                if now % 16 == 0 {
                    pair.a
                        .send(Channel::Unreliable, stamp(1, 0, now, 40), 0)
                        .expect("input");
                    pair.b
                        .send(Channel::Unreliable, stamp(1, 0, now, 200), 0)
                        .expect("state");
                }
                if now % 100 == 0 {
                    pair.a
                        .send(Channel::GAME_RPC, stamp(2, index, now, 60), 0)
                        .expect("rpc");
                    pair.b
                        .send(Channel::GAME_RPC, stamp(2, index, now, 60), 0)
                        .expect("rpc");
                    index += 1;
                }
            }
            pair.step(now);
            for c in [&mut pair.a, &mut pair.b] {
                while let Some((channel, m)) = c.next_message() {
                    if channel == Channel::GAME_RPC && now >= 2_000 {
                        lat.push(now - sent_at(&m));
                    }
                }
            }
        }
        let p95 = percentile(&mut lat, 95);
        let (sa, sb) = (pair.a.stats(), pair.b.stats());
        println!(
            "E2 seed={seed} small_reliable p50={} p95={p95} max={} resends={} lost={} rtt={:.1}",
            percentile(&mut lat, 50),
            lat.last().copied().unwrap_or(0),
            sa.reliable_resends + sb.reliable_resends,
            sa.packets_lost + sb.packets_lost,
            sa.rtt_ms
        );
        worst = worst.max(p95);
        latencies.extend(lat);
    }
    let p95 = percentile(&mut latencies, 95);
    println!("E2 all seeds small_reliable p95={p95} worst_seed_p95={worst}");
    // Each seeded run is one E2 rerun: every one must meet the bar.
    assert!(
        worst * 2 <= ONE_WAY * 3,
        "small reliable p95 {worst} ms (worst seed) is over 1.5x the {ONE_WAY} ms one-way delay"
    );
}

/// Adversarial row 1 (spec 120 § 4.1): a 64-packet burst is acknowledged in full by one reply,
/// so none of the 64 is counted lost or resent (the wire-4 base needed 4 replies).
#[test]
fn a_64_packet_burst_is_fully_acknowledged_by_one_reply() {
    let mut a = Connection::new(blind_link());
    let mut b = Connection::new(blind_link());
    for i in 0..64u32 {
        a.send(Channel::GAME_RPC, stamp(3, i, 0, 1_100), 0)
            .expect("queued");
    }
    let burst = a.poll(0, &[]);
    assert_eq!(burst.len(), 64, "one poll sends the whole burst");
    for p in &burst {
        b.receive(p, 20).expect("valid packet");
    }
    let replies = b.poll(20, &[]);
    println!("burst replies={}", replies.len());
    assert_eq!(replies.len(), 1, "one reply acknowledges the burst");
    a.receive(&replies[0], 40).expect("valid packet");
    let stats = a.stats();
    assert_eq!(stats.packets_acked, 64, "{stats:?}");
    assert_eq!(stats.packets_lost, 0);
    assert!(!a.has_unacked_reliable());
    // Nothing is resent afterwards either.
    for now in 41..2_000 {
        for p in a.poll(now, &[]) {
            b.receive(&p, now).expect("valid packet");
        }
        for p in b.poll(now, &[]) {
            a.receive(&p, now).expect("valid packet");
        }
    }
    assert_eq!(a.stats().reliable_resends, 0);
}

/// DO27 rule A7, "reorder is never loss" (spec 120 § 4.5): on a bad-wifi path with 15 %
/// reordering, a 60 Hz game with a 256 KiB join snapshot counts no delivered packet as lost. At the end, each sender's lost
/// count is at most the packets its link really dropped.
#[test]
fn a7_fifteen_percent_reorder_on_bad_wifi_counts_no_delivered_packet_as_lost() {
    // bad-wifi (net-soak profile): 8 ms, 30 ms jitter, 1 % loss; reordering raised to 15 %.
    let mut pair = Pair::new(link(8, 30, 0.01, 0.15, 30), 7);
    let mut bulk = 0u32;
    for now in 0..70_000u64 {
        // A 256 KiB join snapshot at 5 s: bursts of full packets next to the game traffic.
        if now >= 5_000 {
            while bulk < 256
                && pair
                    .b
                    .send(Channel::GAME_RPC, stamp(3, bulk, now, 1_024), 0)
                    .is_ok()
            {
                bulk += 1;
            }
        }
        if now < 60_000 && now % 16 == 0 {
            pair.a
                .send(Channel::Unreliable, stamp(1, 0, now, 40), 0)
                .expect("input");
            for _ in 0..2 {
                pair.b
                    .send(Channel::Unreliable, stamp(1, 0, now, 400), 0)
                    .expect("state");
            }
            if now % 160 == 0 {
                pair.a
                    .send(Channel::GAME_RPC, stamp(2, 0, now, 100), 0)
                    .expect("rpc");
                pair.b
                    .send(Channel::GAME_RPC, stamp(2, 0, now, 200), 0)
                    .expect("rpc");
            }
        }
        pair.step(now);
        while pair.a.next_message().is_some() {}
        while pair.b.next_message().is_some() {}
    }
    let (sa, sb) = (pair.a.stats(), pair.b.stats());
    // Every packet a sent either reached b (counted once) or was dropped by the link.
    let up_missing = sa.packets_sent - sb.packets_received;
    let down_missing = sb.packets_sent - sa.packets_received;
    println!(
        "reorder up: sent={} received={} missing={up_missing} counted_lost={}; down: sent={} received={} \
         missing={down_missing} counted_lost={}; resends={}",
        sa.packets_sent,
        sb.packets_received,
        sa.packets_lost,
        sb.packets_sent,
        sa.packets_received,
        sb.packets_lost,
        sa.reliable_resends + sb.reliable_resends
    );
    assert!(up_missing > 0 && down_missing > 0, "the link dropped something");
    assert!(
        sa.packets_lost <= up_missing,
        "a counted {} lost, but only {up_missing} never arrived",
        sa.packets_lost
    );
    assert!(
        sb.packets_lost <= down_missing,
        "b counted {} lost, but only {down_missing} never arrived",
        sb.packets_lost
    );
    assert!(!pair.a.has_unacked_reliable() && !pair.b.has_unacked_reliable());
}

/// Probe timeout backoff (spec 120 § 4.1 item 5, RFC 9002 § 6.2): during an 8 s two-way outage a
/// reliable fragment is sent again at most 4 times (the wire-4 base: 5), and it is delivered
/// once the path returns.
#[test]
fn an_eight_second_outage_resends_a_fragment_at_most_four_times() {
    let mut pair = Pair::new(link(40, 0, 0.0, 0.0, 0), 3);
    // Two seconds of keepalives measure the path.
    for now in 0..2_000 {
        pair.step(now);
    }
    pair.a
        .send(Channel::GAME_RPC, stamp(2, 0, 2_000, 100), 0)
        .expect("rpc");
    pair.cut = true;
    for now in 2_000..10_000 {
        pair.step(now);
    }
    let during = pair.a.stats().reliable_resends;
    pair.cut = false;
    let mut delivered = None;
    for now in 10_000..14_000 {
        pair.step(now);
        if delivered.is_none() && pair.b.next_message().is_some() {
            delivered = Some(now);
        }
    }
    println!(
        "outage resends during={during} total={} delivered_at={delivered:?}",
        pair.a.stats().reliable_resends
    );
    assert!(
        during <= 4,
        "{during} resends of one fragment during an 8 s outage"
    );
    let delivered = delivered.expect("delivered after the outage");
    assert!(
        delivered < 11_000,
        "delivered within a second of the path returning ({delivered})"
    );
}

#[derive(Debug, Clone, PartialEq)]
struct Bytes(Vec<u8>);

impl NetCodec for Bytes {
    fn encode(&self, w: &mut WireWriter) {
        self.0.encode(w);
    }
    fn decode(r: &mut WireReader<'_>) -> Result<Self, WireError> {
        Ok(Self(Vec::<u8>::decode(r)?))
    }
}

#[derive(Debug)]
struct Game;
impl NetGame for Game {
    type Message = Bytes;
    type Input = u8;
    type JoinData = ();
    const GAME_ID: &'static str = "test.transport-v2";
    const PROTOCOL_VERSION: u32 = 1;
    const GAME_VERSION: &'static str = "2.0.0";
}

/// A peer built on an older wire is refused at the handshake with `protocol_mismatch`, never
/// silently: a request that carries the old build's version stamp gets the server's stamp back
/// after the deny, and the old build's version probe (a frozen format) is answered with this
/// build's wire version, so either way it can tell the player which side is older (spec 120
/// § 12.1, § 12.2; § 4.5 A11).
fn an_old_wire_peer_is_refused_cleanly_and_its_probe_answered(old_wire: u32) {
    // Wire 6 (ADR-8858): the resumption ticket in the sealed accept and the resume datagrams.
    assert_eq!(regia_net::REGIA_NET_WIRE_VERSION, 6);
    let at = |port: u16| SocketAddr::from(([127, 0, 0, 1], port));
    let net = LoopbackNet::new();
    let mut server: NetServer<Game, regia_net::LoopbackSocket> = NetServer::with_secret(
        net.bind(at(7777)).expect("bind"),
        ServerConfig {
            crypto: regia_net::CryptoMode::Required,
            ..ServerConfig::default()
        },
        [3; 32],
    );
    // The protocol id a build of the same game on the old wire computes (`protocol_id`).
    let mut w = WireWriter::new();
    w.u32(old_wire);
    w.raw(Game::GAME_ID.as_bytes());
    w.u8(0);
    w.u32(Game::PROTOCOL_VERSION);
    let old_id = fnv1a_64(w.as_slice());
    assert_ne!(old_id, regia_net::protocol_id::<Game>());
    let mut old = net.bind(at(4242)).expect("bind old client");
    let request = ConnectRequest {
        protocol_id: old_id,
        client_nonce: 0x0D27,
        crypto_version: 2,
        crypto_required: true,
    };
    old.send_to(&request.to_datagram(), at(7777)).expect("send");
    let mut deny = None;
    let mut buf = [0u8; 2048];
    for step in 1..=100u64 {
        server.update(step);
        if let Some((n, _)) = old.recv_from(&mut buf).expect("receive")
            && let Some((PacketKind::Deny, body)) = split_datagram(&buf[..n])
        {
            deny = Some(Deny::decode(body).expect("deny"));
            break;
        }
    }
    let deny = deny.expect("the server answered");
    assert_eq!(deny.client_nonce, 0x0D27);
    assert_eq!(deny.reason.as_str(), "protocol_mismatch");
    assert_eq!(server.session().len(), 0, "nobody joined");
    // The same request with the old build's stamp: the deny carries the server's stamp, so the
    // old build names the older side without the probe's round trip.
    let stamped = ConnectRequest {
        client_nonce: 0x0D28,
        ..request
    };
    let old_stamp = VersionStamp::new(old_wire, "1.9.0", Game::PROTOCOL_VERSION, 0);
    old.send_to(&stamped.to_datagram_with_stamp(&old_stamp), at(7777))
        .expect("send");
    let mut stamped_deny = None;
    for step in 101..=200u64 {
        server.update(step);
        if let Some((n, _)) = old.recv_from(&mut buf).expect("receive")
            && let Some((PacketKind::Deny, body)) = split_datagram(&buf[..n])
        {
            stamped_deny = Some(Deny::decode_with_stamp(body).expect("deny"));
            break;
        }
    }
    let (deny, server_stamp) = stamped_deny.expect("the server answered");
    assert_eq!(deny.client_nonce, 0x0D28);
    assert_eq!(deny.reason.as_str(), "protocol_mismatch");
    let server_stamp = server_stamp.expect("the server's stamp follows the deny");
    assert_eq!(server_stamp.wire, 6);
    assert_eq!(
        MismatchSide::between(Some(&old_stamp), Some(&server_stamp)),
        MismatchSide::ClientOlder
    );
    assert_eq!(server.session().len(), 0, "nobody joined");
    // The old build's probe (a frozen format) is answered with the server's wire version.
    old.send_to(&ProbeRequest { nonce: 0xBEEF }.to_datagram(), at(7777))
        .expect("send");
    let mut answer = None;
    for step in 201..=300u64 {
        server.update(step);
        if let Some((n, _)) = old.recv_from(&mut buf).expect("receive")
            && let Ok(a) = ProbeAnswer::decode(&buf[..n])
        {
            answer = Some(a);
            break;
        }
    }
    let answer = answer.expect("the probe is answered");
    assert_eq!(answer.nonce, 0xBEEF);
    assert_eq!(answer.stamp.wire, 6);
    assert_eq!(answer.stamp.game_version, "2.0.0");
}

/// A peer built on wire 4 (before transport v2).
#[test]
fn a_wire_version_4_peer_is_refused_cleanly_and_its_probe_answered() {
    an_old_wire_peer_is_refused_cleanly_and_its_probe_answered(4);
}

/// A peer built on wire 5 (the v0.6 line, before the one-round-trip resume of wire 6, ADR-8858).
#[test]
fn a_wire_version_5_peer_is_refused_cleanly_and_its_probe_answered() {
    an_old_wire_peer_is_refused_cleanly_and_its_probe_answered(5);
}
