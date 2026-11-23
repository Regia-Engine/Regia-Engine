// SPDX-License-Identifier: MPL-2.0
//! Path MTU discovery acceptance (spec 120 § 4.3, row T2, plan N205, ADR-8300): DPLPMTUD probes,
//! search, and black-hole fall back, on the public `Connection` API over the crate's seeded
//! conditioner plus a per-direction MTU (a datagram longer than the path MTU is dropped, as with
//! DF set and no ICMP), virtual time in 1 ms steps, no sleeps.
//!
//! Everything is measured from the wire (datagram sizes, which ones are probes, what arrives), and
//! probing is switched on through `LinkConfig`'s serde form, so the same file builds on the base
//! commit (which ignores the unknown key): there it prints the "before" numbers and fails.
use regia_net::conditioner::{ConditionerConfig, NetConditioner};
use regia_net::connection::{Channel, Connection, LinkConfig};
use regia_net::crypto::SEALED_RESERVED_HEADER_BYTES;
use regia_net::wire::fnv1a_64;
use std::net::SocketAddr;

/// The endpoint header every packet carries (a sealed link's reservation).
const H: usize = SEALED_RESERVED_HEADER_BYTES;

fn addr() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 1))
}

fn probing() -> LinkConfig {
    ron::from_str("(mtu_probing: true)").expect("link config")
}

/// A probe on the wire: no acknowledgment frame, a ping, then padding (record tag 0).
fn is_probe(datagram: &[u8]) -> bool {
    let Some(&flags) = datagram.get(H) else {
        return false;
    };
    let pn_len = match flags & 0x03 {
        0 => 1,
        1 => 2,
        _ => 4,
    };
    let at = H + 1 + pn_len;
    flags & 0x04 == 0 && datagram.get(at) == Some(&0x03) && datagram.get(at + 1) == Some(&0x00)
}

/// One datagram a side sent: when, how long, whether it was a probe.
#[derive(Debug, Clone, Copy)]
struct Sent {
    at: u64,
    len: usize,
    probe: bool,
}

/// A path MTU per direction as a function of time.
type MtuAt = fn(u64) -> usize;

struct Pair {
    a: Connection,
    b: Connection,
    up: NetConditioner,
    down: NetConditioner,
    mtu: MtuAt,
    a_sent: Vec<Sent>,
    b_sent: Vec<Sent>,
    /// Every datagram both sides sent, in order, hashed with its send time.
    wire_hash: u64,
}

fn deliver(to: &mut Connection, datagram: &[u8], now: u64) {
    to.receive(&datagram[H..], now).expect("valid packet");
}

impl Pair {
    fn new(config: LinkConfig, link: ConditionerConfig, seed: u64, mtu: MtuAt) -> Self {
        Self {
            a: Connection::new(config.clone()),
            b: Connection::new(config),
            up: NetConditioner::new(ConditionerConfig { seed, ..link.clone() }),
            down: NetConditioner::new(ConditionerConfig {
                seed: seed + 1,
                ..link
            }),
            mtu,
            a_sent: Vec::new(),
            b_sent: Vec::new(),
            wire_hash: 0xcbf2_9ce4_8422_2325,
        }
    }

    fn note(hash: &mut u64, now: u64, p: &[u8]) {
        let mut bytes = now.to_le_bytes().to_vec();
        bytes.extend_from_slice(p);
        *hash = fnv1a_64(&bytes) ^ hash.rotate_left(5);
    }

    fn step(&mut self, now: u64) {
        let header = [0u8; H];
        let mtu = (self.mtu)(now);
        for p in self.a.poll(now, &header) {
            Self::note(&mut self.wire_hash, now, &p);
            self.a_sent.push(Sent {
                at: now,
                len: p.len(),
                probe: is_probe(&p),
            });
            if p.len() > mtu {
                continue;
            }
            for (p, _) in self.up.process(p, addr(), now) {
                deliver(&mut self.b, &p, now);
            }
        }
        for (p, _) in self.up.release_due(now) {
            deliver(&mut self.b, &p, now);
        }
        for p in self.b.poll(now, &header) {
            Self::note(&mut self.wire_hash, now, &p);
            self.b_sent.push(Sent {
                at: now,
                len: p.len(),
                probe: is_probe(&p),
            });
            if p.len() > mtu {
                continue;
            }
            for (p, _) in self.down.process(p, addr(), now) {
                deliver(&mut self.a, &p, now);
            }
        }
        for (p, _) in self.down.release_due(now) {
            deliver(&mut self.a, &p, now);
        }
    }
}

fn link(delay_ms: u64, loss: f64) -> ConditionerConfig {
    ConditionerConfig {
        enabled: true,
        delay_ms,
        loss,
        ..Default::default()
    }
}

fn stamp(kind: u8, index: u32, now: u64, len: usize) -> Vec<u8> {
    let mut m = vec![0u8; len.max(13)];
    m[0] = kind;
    m[1..5].copy_from_slice(&index.to_le_bytes());
    m[5..13].copy_from_slice(&now.to_le_bytes());
    m
}

fn field(m: &[u8]) -> (u8, u32, u64) {
    let mut i = [0u8; 4];
    i.copy_from_slice(&m[1..5]);
    let mut at = [0u8; 8];
    at.copy_from_slice(&m[5..13]);
    (m[0], u32::from_le_bytes(i), u64::from_le_bytes(at))
}

/// What one run of the game-like workload saw.
#[derive(Debug, Default)]
struct Run {
    /// Reliable messages a sent, and the ones b received (index, latency ms), in arrival order.
    reliable_sent: u32,
    reliable_got: Vec<(u32, u64, u64)>,
    /// Unreliable state frames b received: (arrival ms, length).
    state_got: Vec<(u64, usize)>,
}

/// a is a server: a state frame of the largest unreliable size every 16 ms (read live), a 300-byte
/// reliable event every 50 ms and a 3,000-byte reliable message every second. b is a client: a
/// 40-byte input every 16 ms and a 60-byte reliable command every 100 ms.
fn run(pair: &mut Pair, until_ms: u64, traffic_until_ms: u64) -> Run {
    let mut out = Run::default();
    let mut index = 0u32;
    let mut b_index = 0u32;
    for now in 0..until_ms {
        if now < traffic_until_ms {
            if now.is_multiple_of(16) {
                let max = pair.a.max_unreliable_bytes(H);
                pair.a
                    .send(Channel::Unreliable, stamp(1, 0, now, max), H)
                    .expect("state");
                pair.b
                    .send(Channel::Unreliable, stamp(1, 0, now, 40), H)
                    .expect("input");
            }
            if now.is_multiple_of(50) {
                pair.a
                    .send(Channel::GAME_RPC, stamp(2, index, now, 300), H)
                    .expect("event");
                index += 1;
            }
            if now % 1000 == 500 {
                pair.a
                    .send(Channel::GAME_RPC, stamp(2, index, now, 3000), H)
                    .expect("bulk");
                index += 1;
            }
            if now.is_multiple_of(100) {
                pair.b
                    .send(Channel::GAME_RPC, stamp(3, b_index, now, 60), H)
                    .expect("command");
                b_index += 1;
            }
        }
        pair.step(now);
        while let Some((channel, m)) = pair.b.next_message() {
            let (kind, i, at) = field(&m);
            match (channel, kind) {
                (Channel::GAME_RPC, 2) => out.reliable_got.push((i, now, now - at)),
                (Channel::Unreliable, 1) => out.state_got.push((now, m.len())),
                _ => {}
            }
        }
        while pair.a.next_message().is_some() {}
    }
    out.reliable_sent = index;
    out
}

fn largest_data(sent: &[Sent], from: u64, to: u64) -> usize {
    sent.iter()
        .filter(|s| !s.probe && s.at >= from && s.at < to)
        .map(|s| s.len)
        .max()
        .unwrap_or(0)
}

fn assert_every_reliable_in_order(r: &Run) {
    let got: Vec<u32> = r.reliable_got.iter().map(|(i, _, _)| *i).collect();
    let want: Vec<u32> = (0..r.reliable_sent).collect();
    assert_eq!(
        got, want,
        "every reliable message arrives once, in order (0 lost)"
    );
}

fn mtu_1280(_: u64) -> usize {
    1280
}

/// Acceptance, profile mtu-1280: a path that carries 1,280-byte datagrams and nothing larger.
/// Probing settles at 1,280 within the first seconds, never sends a larger data packet, gives up
/// on 1,400 after three lost probes and never tries 1,452.
#[test]
fn mtu_1280_probing_settles_at_1280_and_never_sends_a_larger_data_packet() {
    let mut pair = Pair::new(probing(), link(20, 0.0), 1280, mtu_1280);
    let r = run(&mut pair, 62_000, 60_000);
    let settled = largest_data(&pair.a_sent, 5_000, 62_000);
    let early = largest_data(&pair.a_sent, 0, 5_000);
    let over: Vec<usize> = pair
        .a_sent
        .iter()
        .chain(pair.b_sent.iter())
        .filter(|s| !s.probe && s.len > 1280)
        .map(|s| s.len)
        .collect();
    let probes: Vec<usize> = pair.a_sent.iter().filter(|s| s.probe).map(|s| s.len).collect();
    let first_1280 = pair
        .a_sent
        .iter()
        .find(|s| !s.probe && s.len == 1280)
        .map(|s| s.at);
    println!(
        "mtu-1280: largest data packet 0-5 s {early}, 5-62 s {settled}; first 1280-byte data packet at {first_1280:?} ms; \
         data packets over 1280: {}; a's probes {probes:?}; b's probes {}",
        over.len(),
        pair.b_sent.iter().filter(|s| s.probe).count()
    );
    // Packet headers vary by a few bytes, so a full packet is at most the path MTU: a data packet
    // over 1,200 means the size was raised, none over 1,280 that it stopped there.
    assert!(
        settled > 1200 && settled <= 1280,
        "probing settles at 1,280 (largest data packet after 5 s: {settled})"
    );
    assert!(over.is_empty(), "no data packet larger than 1,280: {over:?}");
    assert_eq!(
        probes.iter().filter(|p| **p == 1400).count(),
        3,
        "three lost 1,400-byte probes end the search"
    );
    assert!(
        !probes.contains(&1452),
        "1,452 is never probed above a failed 1,400"
    );
    assert!(probes.iter().all(|p| *p <= 1452), "probe sizes {probes:?}");
    assert_every_reliable_in_order(&r);
}

fn mtu_blackhole(now: u64) -> usize {
    if now < BLACKHOLE_AT_MS { 1280 } else { 1200 }
}

/// When the path stops carrying datagrams over 1,200 bytes (spec 120 § 15.1 mtu-blackhole).
const BLACKHOLE_AT_MS: u64 = 30_000;

/// Acceptance, profile mtu-blackhole: the path carries 1,280 until t = 30 s, then nothing over
/// 1,200. The sender is back at 1,200 within three probe losses, loses no reliable message, and no
/// reliable message (sent every 50 ms) waits longer than one second; state frames keep arriving
/// and shrink to the new size (the budget is read live).
#[test]
fn mtu_blackhole_falls_back_to_1200_within_three_probe_losses_without_a_stall() {
    let mut pair = Pair::new(probing(), link(20, 0.0), 1201, mtu_blackhole);
    let r = run(&mut pair, 62_000, 60_000);
    let before = largest_data(&pair.a_sent, 5_000, BLACKHOLE_AT_MS);
    let last_large = pair
        .a_sent
        .iter()
        .filter(|s| !s.probe && s.len > 1200)
        .map(|s| s.at)
        .max()
        .unwrap_or(0);
    let probes_after: Vec<(u64, usize)> = pair
        .a_sent
        .iter()
        .filter(|s| s.probe && s.at >= BLACKHOLE_AT_MS)
        .map(|s| (s.at, s.len))
        .collect();
    let stall = r
        .reliable_got
        .iter()
        .filter(|(_, at, _)| *at >= BLACKHOLE_AT_MS - 1_000)
        .map(|(_, _, latency)| *latency)
        .max()
        .unwrap_or(0);
    let state_after: Vec<&(u64, usize)> = r
        .state_got
        .iter()
        .filter(|(at, _)| *at >= BLACKHOLE_AT_MS + 1_000)
        .collect();
    let state_gap = r
        .state_got
        .windows(2)
        .filter(|w| w[1].0 >= BLACKHOLE_AT_MS)
        .map(|w| w[1].0 - w[0].0)
        .max()
        .unwrap_or(u64::MAX);
    let s = pair.a.stats();
    println!(
        "mtu-blackhole: largest data packet 5-30 s {before}; last data packet over 1200 at {last_large} ms; \
         probes after 30 s {probes_after:?}; worst reliable latency from 29 s {stall} ms; \
         reliable {} of {} delivered; state frames after 31 s {} (largest {:?}), worst state gap from 30 s {state_gap} ms; \
         unreliable dropped {}",
        r.reliable_got.len(),
        r.reliable_sent,
        state_after.len(),
        state_after.iter().map(|(_, len)| *len).max(),
        s.unreliable_dropped
    );
    // Debug form only, so the file still builds on the base: path MTU, search state, probe and
    // black-hole counters after the run.
    println!("mtu-blackhole sender stats: {s:?}");
    assert!(
        before > 1200 && before <= 1280,
        "the path MTU was probed to 1,280 before the black hole (largest data packet {before})"
    );
    assert!(
        last_large < BLACKHOLE_AT_MS + 1_000,
        "no data packet over 1,200 later than 1 s after the black hole (last at {last_large} ms)"
    );
    assert!(
        probes_after.len() <= 3,
        "back to 1,200 within three probe losses: {probes_after:?}"
    );
    assert_every_reliable_in_order(&r);
    assert!(stall <= 1_000, "no reliable stall over 1 s ({stall} ms)");
    assert!(
        state_gap <= 1_000,
        "state frames keep flowing ({state_gap} ms gap)"
    );
    assert!(
        state_after
            .iter()
            .all(|(_, len)| *len <= 1200 - H - regia_net::connection::PACKET_HEADER_BYTES - 3),
        "state frames shrank to the 1,200-byte budget"
    );
}

/// Guard: with probing off (the default) the wire is byte-identical to the transport without path
/// MTU discovery. The hash was taken on the base commit (745c4d082 + net-v2-acks-loss, before any
/// path MTU code) with this file; a clean path, so every decision is deterministic.
#[test]
fn probing_off_by_default_leaves_the_clean_wire_byte_identical() {
    let mut pair = Pair::new(LinkConfig::default(), link(20, 0.0), 7, |_| usize::MAX);
    let r = run(&mut pair, 12_000, 10_000);
    let probes = pair.a_sent.iter().chain(&pair.b_sent).filter(|s| s.probe).count();
    println!(
        "off: {} + {} datagrams, wire hash {:#018x}, probes {probes}, largest {}",
        pair.a_sent.len(),
        pair.b_sent.len(),
        pair.wire_hash,
        largest_data(&pair.a_sent, 0, u64::MAX)
    );
    assert_eq!(probes, 0);
    assert_every_reliable_in_order(&r);
    assert_eq!(pair.wire_hash, OFF_WIRE_HASH, "the default wire changed");
}

/// The wire hash of [`probing_off_by_default_leaves_the_clean_wire_byte_identical`] on the base.
/// Retaken for ADR-8423 (acknowledgment frames stop naming confirmed numbers, a smaller frame by
/// design): with that trimming disabled the run still gives the base hash `0x2cf3_e0ed_e012_d7a8`
/// and the same 1,387 + 1,282 datagrams, so the trimming is the only change to this wire.
/// Retaken again for the rate controller (ADR-8301, ADR-8400, on by default; integ-v06q-net,
/// ADR-8487): with `RateConfig::default().enabled` false the run gives the ADR-8423 hash
/// `0xce6b_c6de_dc5f_ecfc` and 1,387 + 1,282 datagrams; with the controller on, 1,383 + 1,282
/// (the test Mac aarch64 and the build server x86-64 alike), so pacing and repair copies are the only change.
/// Retaken for ADR-8592 (the startup reads the pipe only from filled rounds and outlives the
/// app-limited bound's first second): with the pre-ADR-8592 startup and held rules switched back
/// (a scratch switch, the build server x86-64) the run gives the ADR-8487 hash `0x8253_f481_fdf5_cbc8`;
/// with them, the same 1,383 + 1,282 datagrams hash as below, so the pacing of the same packets is
/// the only change.
/// Retaken for ADR-8624 on the v0.6 line (reliable streams' credit records and per-stream record
/// tags, ADR-8302, and the scheduler's send order, ADR-8407, change the default wire by design;
/// neither can be switched off): with the `pmtud` module stubbed out at run time (no probe, no
/// expiry, no size change, no loss or acknowledgment bookkeeping; a scratch switch, the build server
/// x86-64) the run gives the same hash and the same 1,384 + 1,282 datagrams as without the stub,
/// so path MTU code still leaves the default wire alone.
/// Retaken for ADR-8692 (the pacer's burst floor is four packets, it was two): in one binary with
/// the floor switched back to two packets (bucket depths and the essential class's first burst; a
/// scratch switch, the build server x86-64) the run gives the ADR-8624 hash `0x1da5_8fd9_fb37_db95` and
/// 1,384 + 1,282 datagrams; with the floor at four, 1,388 + 1,282, so the pacing of the same
/// traffic is the only change.
/// Retaken for ADR-10050 (wire 6: every acknowledgment frame carries the two bytes of its newest
/// packet's arrival stamp): in one binary with the stamp left out of the frame's encoding and
/// decoding (a scratch switch, the build server x86-64) the run gives the ADR-8692 hash
/// `0x86bf_0110_1268_3c45` and 1,388 + 1,282 datagrams; with the stamp, the same 1,388 + 1,282
/// datagrams hash as below, with ADR-10050's rule on or off, so the stamp is the only change.
const OFF_WIRE_HASH: u64 = 0xf75b_5126_6c88_6073;
