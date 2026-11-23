// SPDX-License-Identifier: MPL-2.0
//! Reliable streams without head-of-line blocking (spec 120 § 5.1, § 5.2, plan row N203,
//! ADR-8302), measured on the public `Connection` API in virtual time (1 ms steps, seeded loss, no
//! sleeps, no wall clock).
//!
//! The bulk and unordered channels are named once, in `BULK` and `UNORDERED`; a run on a base
//! without streams sets both to the game RPC channel (`Channel::GAME_RPC`), the only reliable
//! channel it has.
use std::collections::VecDeque;

use regia_net::connection::{Channel, Connection, LinkConfig, LinkFailure, StreamId};
use regia_net::wire::WireWriter;

/// The default link with the first flight named off (ADR-10156; off by default in v0.7,
/// ADR-10231, and a later release may turn it on): these
/// tests measure the transport's blind start (the 64-packet initial window and what it makes of
/// acknowledgments, bursts and rates), which the first flight replaces until a path answers.
fn blind_link() -> LinkConfig {
    let mut config = LinkConfig::default();
    config.rate.first_flight = false;
    config
}

/// The bulk stream (spec 120 § 5.1 stream 6).
const BULK: Channel = Channel::Stream(StreamId::BULK);
/// The unordered RPC stream (`ReliableUnordered`, stream 7).
const UNORDERED: Channel = Channel::Stream(StreamId::UNORDERED_RPC);
/// An ordered stream other than stream 2 (chat, stream 5).
const CHAT: Channel = Channel::Stream(StreamId::CHAT);

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

fn index_of(m: &[u8]) -> u32 {
    let mut at = [0u8; 4];
    at.copy_from_slice(&m[1..5]);
    u32::from_le_bytes(at)
}

fn percentile(values: &mut [u64], p: usize) -> u64 {
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    values[(values.len() - 1) * p / 100]
}

/// A deterministic loss source (64-bit LCG).
struct Lcg(u64);

impl Lcg {
    fn chance(&mut self, p: f64) -> bool {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((self.0 >> 11) as f64 / (1u64 << 53) as f64) < p
    }
}

/// One direction of a capped path: random loss, a bottleneck serving `bytes_per_ms` behind a
/// drop-tail queue of `queue_ms`, then `delay_ms` of propagation (the audit's E2b link).
struct Bottleneck {
    rng: Lcg,
    loss: f64,
    bytes_per_ms: f64,
    queue_ms: f64,
    delay_ms: u64,
    /// When the bottleneck finishes what it already holds (virtual ms, fractional).
    busy_until: f64,
    in_flight: VecDeque<(u64, Vec<u8>)>,
    queue_drops: u64,
}

impl Bottleneck {
    fn new(seed: u64, loss: f64, bytes_per_ms: f64, queue_ms: f64, delay_ms: u64) -> Self {
        Self {
            rng: Lcg(seed),
            loss,
            bytes_per_ms,
            queue_ms,
            delay_ms,
            busy_until: 0.0,
            in_flight: VecDeque::new(),
            queue_drops: 0,
        }
    }

    fn send(&mut self, now: u64, packet: Vec<u8>) {
        if self.rng.chance(self.loss) {
            return;
        }
        let start = self.busy_until.max(now as f64);
        if start - now as f64 > self.queue_ms {
            self.queue_drops += 1;
            return;
        }
        self.busy_until = start + (packet.len() + 28) as f64 / self.bytes_per_ms;
        let arrive = self.busy_until.ceil() as u64 + self.delay_ms;
        // Arrivals stay in order: one queue, one link.
        self.in_flight.push_back((arrive, packet));
    }

    fn due(&mut self, now: u64) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        while self.in_flight.front().is_some_and(|(at, _)| *at <= now) {
            if let Some((_, p)) = self.in_flight.pop_front() {
                out.push(p);
            }
        }
        out
    }
}

/// Small reliable RPC latencies (ms) while a 256 KiB bulk transfer is in flight, or over the
/// same window without it, on the E2b path: 128 KB/s, 250 ms drop-tail queue, 2 % loss, 30 ms
/// one way (both directions). Returns (latencies, when the bulk transfer arrived).
fn e2b(seed: u64, with_bulk: bool) -> (Vec<u64>, Option<u64>) {
    let mut a = Connection::new(LinkConfig::default());
    let mut b = Connection::new(LinkConfig::default());
    let mut down = Bottleneck::new(seed, 0.02, 128.0, 250.0, 30);
    let mut up = Bottleneck::new(seed ^ 0x5555, 0.02, 128.0, 250.0, 30);
    let (bulk_at, rpc_from, rpc_to) = (1_000u64, 1_000u64, 4_000u64);
    let mut latencies = Vec::new();
    let mut bulk_done = None;
    let mut index = 0u32;
    for now in 0..12_000u64 {
        if with_bulk && now == bulk_at {
            a.send(BULK, stamp(2, 0, now, 256 * 1024), 0)
                .expect("bulk queued");
        }
        // A small gameplay RPC every 16 ms (about one per 60 Hz tick).
        if (rpc_from..rpc_to).contains(&now) && now.is_multiple_of(16) {
            a.send(Channel::GAME_RPC, stamp(1, index, now, 40), 0)
                .expect("rpc queued");
            index += 1;
        }
        for p in a.poll(now, &[]) {
            down.send(now, p);
        }
        for p in down.due(now) {
            b.receive(&p, now).expect("valid packet");
        }
        while let Some((_, m)) = b.next_message() {
            match m[0] {
                1 => latencies.push(now - sent_at(&m)),
                _ => bulk_done = Some(now),
            }
        }
        for p in b.poll(now, &[]) {
            up.send(now, p);
        }
        for p in up.due(now) {
            a.receive(&p, now).expect("valid packet");
        }
    }
    assert_eq!(latencies.len() as u32, index, "every RPC arrived");
    (latencies, bulk_done)
}

/// E2b (spec 120 § 15.2 row T4, `rpc_p95_with_bulk`): small RPC p95 with a 256 KiB bulk transfer
/// in flight on the capped path stays within 1.5x of the no-transfer control (the audit measured
/// 30 -> 2,880 ms, 96x, with one ordered stream), and the transfer still completes.
#[test]
fn e2b_small_rpc_p95_with_a_256_kib_bulk_transfer_within_one_and_a_half_times_the_control() {
    for seed in [11u64, 12, 13] {
        let (mut control, _) = e2b(seed, false);
        let (mut loaded, done) = e2b(seed, true);
        let (c95, l95) = (percentile(&mut control, 95), percentile(&mut loaded, 95));
        let lmax = loaded.iter().copied().max().unwrap_or(0);
        println!(
            "seed {seed}: control p95 {c95} ms; with bulk p95 {l95} ms, max {lmax} ms; bulk arrived at {done:?}"
        );
        assert!(
            l95 * 2 <= c95 * 3,
            "seed {seed}: small RPC p95 {l95} ms with bulk vs {c95} ms control (bar 1.5x)"
        );
        // 256 KiB at 128 KB/s is 2 s of the link; it may take longer, never forever.
        let done = done.expect("the bulk transfer completed");
        assert!(done < 12_000, "bulk arrived at {done}");
    }
}

/// Moves `a`'s packets to `b` (no delay), dropping the ones `drop` picks by index, and `b`'s
/// replies back.
fn pump(a: &mut Connection, b: &mut Connection, now: u64, drop: &mut dyn FnMut(usize) -> bool) {
    for (i, p) in a.poll(now, &[]).into_iter().enumerate() {
        if !drop(i) {
            b.receive(&p, now).expect("valid packet");
        }
    }
    for p in b.poll(now, &[]) {
        a.receive(&p, now).expect("valid packet");
    }
}

/// Spec 120 § 5.1: a lost bulk fragment never delays a stream-2 message. The first packet of a
/// 64 KiB bulk transfer is lost; every game RPC sent after it is delivered on arrival while the
/// bulk stream still waits on its gap, and `delivered_past_gap` counts each one.
#[test]
fn a_lost_bulk_fragment_never_delays_a_stream_2_message() {
    let mut a = Connection::new(LinkConfig::default());
    let mut b = Connection::new(LinkConfig::default());
    // Warm the path so the bulk stream has a measured window.
    for now in 0..50 {
        pump(&mut a, &mut b, now, &mut |_| false);
    }
    a.send(BULK, stamp(2, 0, 50, 64 * 1024), 0).expect("bulk queued");
    // Every transmission of the bulk transfer's first fragment is lost until t = 60 (its record:
    // stream 6, first and more flags, id 0).
    let carries_first_bulk_fragment = |p: &[u8]| p.windows(3).any(|w| w == [0x40 | 6 | 0x20 | 0x80, 0, 0]);
    let mut delivered = 0;
    for (i, now) in (50..60u64).enumerate() {
        if now > 50 {
            a.send(Channel::GAME_RPC, stamp(1, i as u32, now, 24), 0)
                .expect("rpc queued");
        }
        for p in a.poll(now, &[]) {
            if !carries_first_bulk_fragment(&p) {
                b.receive(&p, now).expect("valid packet");
            }
        }
        for p in b.poll(now, &[]) {
            a.receive(&p, now).expect("valid packet");
        }
        while let Some((channel, m)) = b.next_message() {
            assert_eq!(m[0], 1, "only the RPCs arrive while the bulk gap is open");
            assert_eq!(channel, Channel::GAME_RPC);
            assert_eq!(sent_at(&m), now, "RPC {} waited", index_of(&m));
            delivered += 1;
        }
    }
    assert_eq!(
        delivered, 9,
        "every RPC was delivered in the poll that carried it"
    );
    let bulk = b.stream_status(StreamId::BULK).expect("bulk stream");
    assert!(bulk.buffered_bytes > 0, "the bulk gap is still open");
    assert!(b.stats().delivered_past_gap >= 9, "{:?}", b.stats());
    // The gap is repaired later and the transfer completes once.
    let mut bulk_messages = 0;
    for now in 60..3_000 {
        pump(&mut a, &mut b, now, &mut |_| false);
        while let Some((channel, m)) = b.next_message() {
            assert_eq!(channel, BULK);
            assert_eq!(m.len(), 64 * 1024);
            bulk_messages += 1;
        }
    }
    assert_eq!(bulk_messages, 1);
}

/// `ReliableUnordered` really is unordered (spec 120 § 5.1 stream 7): with the first fragment of a
/// three-fragment message lost, the small message sent after it is delivered first, and the big
/// one follows once repaired; each arrives exactly once.
#[test]
fn reliable_unordered_delivers_out_of_order_when_a_gap_exists() {
    let mut a = Connection::new(LinkConfig::default());
    let mut b = Connection::new(LinkConfig::default());
    for now in 0..20 {
        pump(&mut a, &mut b, now, &mut |_| false);
    }
    a.send(UNORDERED, stamp(1, 0, 20, 3_000), 0).expect("big queued");
    a.send(UNORDERED, stamp(1, 1, 20, 40), 0).expect("small queued");
    let mut first = true;
    pump(&mut a, &mut b, 20, &mut |i| {
        let lose = first && i == 0;
        first = false;
        lose
    });
    let mut order = Vec::new();
    for now in 20..2_000 {
        if now > 20 {
            pump(&mut a, &mut b, now, &mut |_| false);
        }
        while let Some((channel, m)) = b.next_message() {
            assert_eq!(channel, UNORDERED);
            order.push((index_of(&m), m.len()));
        }
    }
    assert_eq!(
        order,
        vec![(1, 40), (0, 3_000)],
        "the small message overtook the gap"
    );
    assert!(b.stats().delivered_past_gap >= 1);
}

/// An ordered stream other than stream 2 keeps its order (and its own gap does not hold stream 2).
#[test]
fn an_ordered_stream_keeps_send_order_under_loss() {
    // 100 chat messages per second, past the chat class cap of 30 (ADR-8407): the test is about order.
    let mut a = Connection::new(LinkConfig {
        message_caps: false,
        ..LinkConfig::default()
    });
    let mut b = Connection::new(LinkConfig::default());
    let mut rng = Lcg(77);
    let mut got = Vec::new();
    for now in 0..6_000u64 {
        if now < 2_000 && now.is_multiple_of(10) {
            a.send(CHAT, stamp(5, (now / 10) as u32, now, 300), 0)
                .expect("queued");
        }
        pump(&mut a, &mut b, now, &mut |_| rng.chance(0.2));
        while let Some((channel, m)) = b.next_message() {
            assert_eq!(channel, CHAT);
            got.push(index_of(&m));
        }
    }
    assert_eq!(got, (0..200).collect::<Vec<_>>());
}

/// A forged packet body (wire 5, packet number `pn` in four bytes, no ack frame) with `records`.
fn forged(pn: u32, records: &[u8]) -> Vec<u8> {
    let mut w = WireWriter::new();
    w.u8(2);
    w.u32(pn);
    w.raw(records);
    w.finish()
}

/// One reliable fragment record on `stream` (not stream 2: the `0x40 | stream` form).
fn fragment(stream: u8, id: u16, first: bool, more: bool, bytes: &[u8]) -> Vec<u8> {
    let mut w = WireWriter::new();
    w.u8(0x40 | stream | if first { 0x20 } else { 0 } | if more { 0x80 } else { 0 });
    w.u16(id);
    w.u16(bytes.len() as u16);
    w.raw(bytes);
    w.finish()
}

/// Spec 120 § 5.2: fragments stored ahead of a gap on one stream are capped by the stream's
/// window (chat: 256 KiB), on every fragment stored, and passing it is the named protocol error
/// `stream_over_cap` (the connection-wide cap, 1.2 MB here, is not reached).
#[test]
fn a_stream_window_breach_is_a_named_protocol_error() {
    let mut b = Connection::new(LinkConfig::default());
    let chunk = vec![7u8; 1_100];
    let mut failed = None;
    // Id 0 never comes; ids 1.. pile up behind it.
    for id in 1..400u16 {
        let body = forged(
            u32::from(id),
            &fragment(StreamId::CHAT.0, id, false, true, &chunk),
        );
        if let Err(e) = b.receive(&body, u64::from(id)) {
            failed = Some((id, e));
            break;
        }
        assert!(b.reliable_buffered_bytes() <= 256 * 1024);
    }
    let (id, _) = failed.expect("the breach fails the connection");
    assert_eq!(
        usize::from(id),
        256 * 1024 / 1_100 + 1,
        "refused at the first fragment past the window"
    );
    assert_eq!(b.failure(), Some(LinkFailure::StreamOverCap));
    assert_eq!(LinkFailure::StreamOverCap.as_str(), "stream_over_cap");
    assert_eq!(b.reliable_buffered_bytes(), 0, "the stored fragments are freed");
    assert_eq!(
        regia_net::server::RejectKind::from(LinkFailure::StreamOverCap).as_str(),
        "stream_over_cap"
    );
}

/// A fragment on a stream the table does not declare is the named protocol error `unknown_stream`.
#[test]
fn a_fragment_on_an_undeclared_stream_is_a_named_protocol_error() {
    let mut b = Connection::new(LinkConfig::default());
    let body = forged(0, &fragment(20, 0, true, false, b"hi"));
    assert!(b.receive(&body, 0).is_err());
    assert_eq!(b.failure(), Some(LinkFailure::UnknownStream));
    assert_eq!(LinkFailure::UnknownStream.as_str(), "unknown_stream");
}

/// Spec 120 § 5.2 stall detection is per stream: a gap left open on the chat stream fails the
/// connection after `max(4 x RTO, 2 s)` of the peer's traffic even while stream 2 keeps making
/// progress in every packet.
#[test]
fn a_stalled_stream_fails_named_while_another_stream_flows() {
    let mut b = Connection::new(LinkConfig::default());
    // Chat id 1 arrives, id 0 never does.
    let body = forged(0, &fragment(StreamId::CHAT.0, 1, true, false, b"later"));
    b.receive(&body, 0).expect("stored ahead of the gap");
    let mut failed_at = None;
    for pn in 1..400u32 {
        let now = u64::from(pn) * 20;
        // Stream 2 (short form 0x02): id pn - 1, in order, every packet.
        let mut w = WireWriter::new();
        w.u8(0x02);
        w.u16((pn - 1) as u16);
        w.u16(2);
        w.raw(b"ok");
        if b.receive(&forged(pn, &w.finish()), now).is_err() {
            failed_at = Some(now);
            break;
        }
        while b.next_message().is_some() {}
    }
    let at = failed_at.expect("the stalled chat stream fails the connection");
    assert!(at >= b.reassembly_stall_ms(), "failed at {at} ms");
    assert!(at <= b.reassembly_stall_ms() + 40, "failed at {at} ms");
    assert_eq!(b.failure(), Some(LinkFailure::ReassemblyStalled));
}

/// Flow control (RFC 9000 § 4 `MAX_STREAM_DATA`): an honest sender never trips a stream window.
/// Four 256 KiB messages on the chat stream (window 256 KiB) over a lossy path arrive whole and in
/// order; the sender waited on credit and the receiver granted more as it consumed.
#[test]
fn an_honest_sender_never_trips_a_stream_window() {
    let mut a = Connection::new(blind_link());
    let mut b = Connection::new(blind_link());
    for i in 0..4u32 {
        a.send(CHAT, stamp(5, i, 0, 256 * 1024), 0).expect("queued");
    }
    let mut rng = Lcg(5);
    let mut got = Vec::new();
    for now in 0..20_000u64 {
        for p in a.poll(now, &[]) {
            if !rng.chance(0.1) {
                b.receive(&p, now)
                    .expect("an honest sender never fails the receiver");
            }
        }
        for p in b.poll(now, &[]) {
            if !rng.chance(0.1) {
                a.receive(&p, now).expect("valid packet");
            }
        }
        while let Some((_, m)) = b.next_message() {
            assert_eq!(m.len(), 256 * 1024);
            got.push(index_of(&m));
        }
        if got.len() == 4 {
            break;
        }
    }
    assert_eq!(got, vec![0, 1, 2, 3]);
    assert!(b.failure().is_none());
    assert!(b.stats().credits_sent > 0, "the receiver granted credit");
    assert!(a.stats().stream_blocked > 0, "the sender waited on credit");
}

/// The game declares its own streams (ids 8 and up): an unordered one delivers past a gap like
/// stream 7, and a send on an id past the table is refused.
#[test]
fn a_game_declared_stream_works_and_an_undeclared_one_is_refused() {
    use regia_net::connection::{SendError, StreamSpec};
    let config = LinkConfig {
        streams: vec![StreamSpec::unordered(1)],
        ..LinkConfig::default()
    };
    let mut a = Connection::new(config.clone());
    let mut b = Connection::new(config);
    let game = Channel::Stream(StreamId(StreamId::FIRST_GAME));
    assert_eq!(a.streams().len(), 9);
    assert_eq!(
        a.send(Channel::Stream(StreamId(9)), vec![1], 0),
        Err(SendError::UnknownStream(9))
    );
    a.send(game, stamp(8, 0, 0, 2_500), 0).expect("queued");
    a.send(game, stamp(8, 1, 0, 30), 0).expect("queued");
    let mut first = true;
    pump(&mut a, &mut b, 0, &mut |i| {
        let lose = first && i == 0;
        first = false;
        lose
    });
    let mut order = Vec::new();
    for now in 0..2_000 {
        if now > 0 {
            pump(&mut a, &mut b, now, &mut |_| false);
        }
        while let Some((channel, m)) = b.next_message() {
            assert_eq!(channel, game);
            order.push(index_of(&m));
        }
    }
    assert_eq!(order, vec![1, 0]);
}

/// RFC 9000 § 13.3: lost flow-control credit is sent again. The receiver's first three packets
/// that carry credit for the chat stream are lost; it has nothing else to send but
/// acknowledgments (never acknowledged themselves, so they reveal no loss), yet the grant is
/// repeated after a probe timeout and the 512 KiB on a 256 KiB window still arrive.
#[test]
fn lost_credit_is_sent_again_when_the_receiver_only_acknowledges() {
    let mut a = Connection::new(LinkConfig::default());
    let mut b = Connection::new(LinkConfig::default());
    for i in 0..2u32 {
        a.send(CHAT, stamp(5, i, 0, 256 * 1024), 0).expect("queued");
    }
    // A credit record for stream 5: tag 0x04, then the stream id.
    let carries_credit = |p: &[u8]| p.windows(2).any(|w| w == [0x04, StreamId::CHAT.0]);
    let mut lost_credits = 0;
    let mut got = Vec::new();
    for now in 0..20_000u64 {
        for p in a.poll(now, &[]) {
            b.receive(&p, now).expect("valid packet");
        }
        for p in b.poll(now, &[]) {
            if lost_credits < 3 && carries_credit(&p) {
                lost_credits += 1;
                continue;
            }
            a.receive(&p, now).expect("valid packet");
        }
        while let Some((_, m)) = b.next_message() {
            got.push(index_of(&m));
        }
        if got.len() == 2 {
            break;
        }
    }
    assert_eq!(lost_credits, 3, "the credit packets were lost");
    assert_eq!(
        got,
        vec![0, 1],
        "both messages arrived after the credit was repeated"
    );
    assert!(b.stats().credits_sent >= 4);
}

/// ADR-8624: an honest bulk sender holding the rest of a message until its acknowledgments come
/// back never stalls the receiver, even before the receiver has a round-trip sample. The join of
/// `asym-bufferbloat` turned around (a 2.5 s queue on the uplink) failed its clients exactly so:
/// a 16 KiB world blob on the bulk stream left in its first window (four packets), the rest
/// waited for acknowledgments that crossed the queue, the state frames kept arriving, and the
/// client's stall clock ran out at its 2 s floor (no sample yet, so the RTO was the minimum):
/// `reassembly_stalled`, a protocol error, a rejoin. At ADR-8624 this failed (t = 2,030 ms, srtt
/// unmeasured, 4,744 bytes assembling). ADR-8658: before its first sample a receiver's round trip
/// is at least as long as its oldest unanswered packet has waited (its first keepalive left at
/// t = 0), so the deadline's RTO is never below that wait (up to `resend_max_ms`); the sample
/// that comes back at about 2.56 s sets the measured deadline.
#[test]
fn a_bulk_message_waiting_for_slow_acknowledgments_never_stalls_a_fresh_link() {
    let mut a = Connection::new(LinkConfig::default());
    let mut b = Connection::new(LinkConfig::default());
    // a -> b: 30 ms; b -> a: 30 ms plus 2.5 s of queue.
    let (down_ms, up_ms) = (30u64, 2_530u64);
    let mut down: VecDeque<(u64, Vec<u8>)> = VecDeque::new();
    let mut up: VecDeque<(u64, Vec<u8>)> = VecDeque::new();
    a.send(BULK, stamp(2, 0, 0, 16 * 1024), 0).expect("blob queued");
    let mut delivered = None;
    for now in 0..15_000u64 {
        // A state frame every 16 ms keeps the peer's traffic flowing.
        if now.is_multiple_of(16) {
            a.send(Channel::Unreliable, vec![7u8; 200], 0)
                .expect("state frame queued");
        }
        for p in a.poll(now, &[]) {
            down.push_back((now + down_ms, p));
        }
        while down.front().is_some_and(|(at, _)| *at <= now) {
            let Some((_, p)) = down.pop_front() else { break };
            if let Err(e) = b.receive(&p, now) {
                panic!("t {now}: the receiver failed the link ({e}, {:?})", b.failure());
            }
        }
        while let Some((channel, m)) = b.next_message() {
            if channel == BULK {
                delivered = Some(now);
                assert_eq!(m.len(), 16 * 1024);
            }
        }
        for p in b.poll(now, &[]) {
            up.push_back((now + up_ms, p));
        }
        while up.front().is_some_and(|(at, _)| *at <= now) {
            let Some((_, p)) = up.pop_front() else { break };
            a.receive(&p, now).expect("valid packet");
        }
    }
    assert!(b.failure().is_none(), "{:?}", b.failure());
    let at = delivered.expect("the blob arrived");
    // Its first window at once, the rest after one round trip of 2.56 s, in a few rounds.
    assert!(at < 12_000, "the blob arrived at {at} ms");
}

/// KI-NT-023 (ADR-10077; ADR-7896's tradeoff, measured on wire 6): a reliable input that loses a
/// fragment no longer waits one resend timeout (RFC 6298, 100 ms floor) and no longer holds later
/// reliable messages back. The reliable input lane rides the unordered inputs stream (spec 120
/// § 5.1), and a lost fragment is repaired by RFC 9002 loss detection, about one round trip. Here a
/// 4 KiB input loses its first fragment once on a 20 ms round trip: every small input sent after
/// it arrives in the poll that carries it, and the big one arrives well inside the old floor.
#[test]
fn a_reliable_input_that_loses_a_fragment_is_repaired_in_about_a_round_trip() {
    const ONE_WAY_MS: u64 = 10;
    let inputs = Channel::Stream(StreamId::INPUTS);
    let mut a = Connection::new(LinkConfig::default());
    let mut b = Connection::new(LinkConfig::default());
    let mut to_b: VecDeque<(u64, Vec<u8>)> = VecDeque::new();
    let mut to_a: VecDeque<(u64, Vec<u8>)> = VecDeque::new();
    let mut dropped = false;
    let mut big_at = None;
    let mut small = Vec::new();
    let start = 300;
    // The first fragment of the big input (stream 1, first and more flags, id 0).
    let first_big_fragment = |p: &[u8]| p.windows(3).any(|w| w == [0x40 | 1 | 0x20 | 0x80, 0, 0]);
    for now in 0..1_000u64 {
        if now == start {
            a.send(inputs, stamp(9, 0, now, 4 * 1024), 0)
                .expect("big input queued");
        }
        if now > start && now < start + 100 && now.is_multiple_of(16) {
            a.send(inputs, stamp(1, now as u32, now, 40), 0)
                .expect("input queued");
        } else if now.is_multiple_of(16) {
            a.send(Channel::GAME_RPC, stamp(2, now as u32, now, 40), 0)
                .expect("rpc queued");
        }
        for p in a.poll(now, &[]) {
            if now >= start && !dropped && first_big_fragment(&p) {
                dropped = true;
                continue;
            }
            to_b.push_back((now + ONE_WAY_MS, p));
        }
        while to_b.front().is_some_and(|(at, _)| *at <= now) {
            let (_, p) = to_b.pop_front().expect("front");
            b.receive(&p, now).expect("valid packet");
        }
        for p in b.poll(now, &[]) {
            to_a.push_back((now + ONE_WAY_MS, p));
        }
        while to_a.front().is_some_and(|(at, _)| *at <= now) {
            let (_, p) = to_a.pop_front().expect("front");
            a.receive(&p, now).expect("valid packet");
        }
        while let Some((channel, m)) = b.next_message() {
            match m[0] {
                9 => big_at = Some(now),
                1 => {
                    assert_eq!(channel, inputs);
                    small.push(now - sent_at(&m));
                }
                _ => {}
            }
        }
    }
    assert!(dropped, "the big input's first fragment was lost once");
    let waited = big_at.expect("the big input arrived") - start;
    assert!(
        waited < 100 - ONE_WAY_MS,
        "the lost fragment was repaired in {waited} ms; the RTO floor alone is 100 ms"
    );
    assert!(!small.is_empty());
    assert!(
        small.iter().all(|&w| w <= ONE_WAY_MS + 1),
        "inputs after the gap waited {small:?} ms"
    );
}
