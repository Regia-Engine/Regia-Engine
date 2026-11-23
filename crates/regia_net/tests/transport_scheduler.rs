// SPDX-License-Identifier: MPL-2.0
//! The per-poll scheduler (spec 120 § 5.3) and the per-class reliable message caps (§ 4.5 A2),
//! ADR-8407, on the public `Connection` API in virtual time (fixed steps, no loss unless said, no
//! sleeps, no wall clock).

use std::collections::VecDeque;

use regia_net::connection::{Channel, Connection, LinkConfig, StreamId, StreamSpec};

const REPLICATION: Channel = Channel::Stream(StreamId::REPLICATION);
const BULK: Channel = Channel::Stream(StreamId::BULK);

/// Polls `from` once at `now` and hands every packet to `to`; returns the packets.
fn pump(from: &mut Connection, to: &mut Connection, now: u64) -> usize {
    let packets = from.poll(now, &[]);
    for p in &packets {
        to.receive(p, now).expect("valid packet");
    }
    packets.len()
}

/// Two streams of one priority share the new-data room in turn (weighted round robin, weight 1
/// each): four messages queued on stream 2 and then four on stream 3 leave interleaved, not
/// stream 2's four first.
#[test]
fn streams_of_one_priority_take_turns() {
    let mut a = Connection::new(LinkConfig::default());
    let mut b = Connection::new(LinkConfig::default());
    for i in 0..4u8 {
        a.send(Channel::GAME_RPC, vec![2, i], 0).expect("queue rpc");
    }
    for i in 0..4u8 {
        a.send(REPLICATION, vec![3, i], 0).expect("queue replication");
    }
    pump(&mut a, &mut b, 0);
    let order: Vec<u8> = std::iter::from_fn(|| b.next_message())
        .map(|(_, m)| m[0])
        .collect();
    assert_eq!(
        order,
        vec![2, 3, 2, 3, 2, 3, 2, 3],
        "round robin inside priority 2"
    );
}

/// A weight-2 stream sends two fragments for each one of a weight-1 stream of its priority.
#[test]
fn a_heavier_stream_gets_more_turns() {
    // Game streams 8 (weight 1) and 9 (weight 2), both at priority 2.
    let config = LinkConfig {
        streams: vec![StreamSpec::ordered(2), StreamSpec::ordered(2).with_weight(2)],
        ..LinkConfig::default()
    };
    let mut a = Connection::new(config.clone());
    let mut b = Connection::new(config);
    let (light, heavy) = (Channel::Stream(StreamId(8)), Channel::Stream(StreamId(9)));
    for i in 0..4u8 {
        a.send(light, vec![8, i], 0).expect("queue light");
        a.send(heavy, vec![9, i], 0).expect("queue heavy");
    }
    pump(&mut a, &mut b, 0);
    let order: Vec<u8> = std::iter::from_fn(|| b.next_message())
        .map(|(_, m)| m[0])
        .collect();
    assert_eq!(
        order,
        vec![8, 9, 9, 8, 9, 9, 8, 8],
        "weight 2 takes two turns per round"
    );
}

/// "Other unreliable" (state frames, voice) goes before bulk (spec 120 § 5.3): with a bulk
/// backlog queued first, the first packet of the poll carries the state frame.
#[test]
fn other_unreliable_goes_before_bulk() {
    let mut a = Connection::new(LinkConfig::default());
    let mut b = Connection::new(LinkConfig::default());
    a.send(BULK, vec![6; 4000], 0).expect("queue bulk");
    a.send(Channel::Unreliable, vec![0xA5; 300], 0)
        .expect("queue state frame");
    let packets = a.poll(0, &[]);
    assert!(!packets.is_empty());
    b.receive(&packets[0], 0).expect("first packet");
    let first: Vec<Channel> = std::iter::from_fn(|| b.next_message()).map(|(c, _)| c).collect();
    assert!(
        first.contains(&Channel::Unreliable),
        "the first packet of the poll carries the state frame ahead of bulk: {first:?}"
    );
}

/// Essential room in every poll: a reliable backlog never pushes an input out of the poll's
/// first packet.
#[test]
fn essential_data_leads_every_poll_under_a_reliable_backlog() {
    let mut a = Connection::new(LinkConfig::default());
    let mut b = Connection::new(LinkConfig::default());
    for i in 0..64u8 {
        a.send(Channel::GAME_RPC, vec![i; 900], 0).expect("queue");
    }
    for now in (0..2_000u64).step_by(16) {
        a.send_essential(vec![0xE5, (now / 16) as u8], 0).expect("input");
        let packets = a.poll(now, &[]);
        assert!(!packets.is_empty(), "t={now}: the poll sends");
        b.receive(&packets[0], now).expect("first packet");
        let got: Vec<(Channel, Vec<u8>)> = std::iter::from_fn(|| b.next_message()).collect();
        assert!(
            got.iter().any(|(c, m)| *c == Channel::Unreliable && m[0] == 0xE5),
            "t={now}: the input rides the poll's first packet"
        );
        for p in &packets[1..] {
            b.receive(p, now).expect("packet");
        }
        while b.next_message().is_some() {}
        for p in b.poll(now, &[]) {
            a.receive(&p, now).expect("ack");
        }
    }
}

/// Spec 120 § 4.5 A2: 1,000 reliable messages queued in one tick on the game RPC class leave at
/// most the class's cap per second (240 by default), the rest wait in the queue (none is lost),
/// and the stream row names the class that capped them.
#[test]
fn a_thousand_reliable_messages_in_one_tick_are_capped_per_class() {
    let mut a = Connection::new(LinkConfig::default());
    let mut b = Connection::new(LinkConfig::default());
    for i in 0..1_000u32 {
        a.send(Channel::GAME_RPC, i.to_le_bytes().to_vec(), 0)
            .expect("queue");
    }
    let mut delivered_at: Vec<u64> = Vec::new();
    for now in (0..6_000u64).step_by(10) {
        pump(&mut a, &mut b, now);
        while let Some((channel, _)) = b.next_message() {
            assert_eq!(channel, Channel::GAME_RPC);
            delivered_at.push(now);
        }
        pump(&mut b, &mut a, now);
    }
    let cap = regia_net::connection::DEFAULT_STREAM_MSGS_PER_S as usize;
    // Every one-second window of the run (stepped by the poll interval).
    for start in (0..5_000u64).step_by(10) {
        let n = delivered_at
            .iter()
            .filter(|t| **t >= start && **t < start + 1_000)
            .count();
        assert!(
            n <= cap,
            "{n} game RPC messages delivered in [{start}, {}) ms (cap {cap}/s)",
            start + 1_000
        );
    }
    assert_eq!(delivered_at.len(), 1_000, "nothing is lost, only paced");
    let row = a.stream_status(StreamId::GAME_RPC).expect("stream 2 row");
    assert!(row.msgs_capped > 0, "{row:?}");
    assert_eq!(row.class.name(), "game_rpc");
}

/// The cap is per class: a capped game RPC backlog does not hold the replication stream back.
#[test]
fn one_class_at_its_cap_does_not_hold_another() {
    let mut a = Connection::new(LinkConfig::default());
    let mut b = Connection::new(LinkConfig::default());
    for i in 0..600u32 {
        a.send(Channel::GAME_RPC, i.to_le_bytes().to_vec(), 0)
            .expect("queue");
    }
    let mut replication = 0;
    for now in (0..1_000u64).step_by(10) {
        a.send(REPLICATION, vec![3; 40], 0).expect("replication frame");
        pump(&mut a, &mut b, now);
        while let Some((channel, _)) = b.next_message() {
            if channel == REPLICATION {
                replication += 1;
            }
        }
        pump(&mut b, &mut a, now);
    }
    assert_eq!(
        replication, 100,
        "every replication frame arrived in its poll's second"
    );
    assert_eq!(
        a.stream_status(StreamId::REPLICATION).expect("row").msgs_capped,
        0
    );
}

/// A deterministic jitter source (64-bit LCG).
struct Lcg(u64);

impl Lcg {
    fn below(&mut self, n: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % n.max(1)
    }
}

/// One direction of the soak harness's `congested` profile in virtual ms: a bottleneck serving
/// `bytes_per_ms` behind a drop-tail queue of `queue_bytes`, then 15 ms plus 0-10 ms of jitter
/// (arrivals stay in order: one queue, one link).
struct Bottleneck {
    rng: Lcg,
    bytes_per_ms: f64,
    queue_bytes: f64,
    busy_until: f64,
    last_arrival: u64,
    in_flight: VecDeque<(u64, Vec<u8>)>,
}

impl Bottleneck {
    fn new(seed: u64, bits_per_second: f64, queue_bytes: f64) -> Self {
        Self {
            rng: Lcg(seed),
            bytes_per_ms: bits_per_second / 8_000.0,
            queue_bytes,
            busy_until: 0.0,
            last_arrival: 0,
            in_flight: VecDeque::new(),
        }
    }

    fn send(&mut self, now: u64, packet: Vec<u8>) {
        let start = self.busy_until.max(now as f64);
        if (start - now as f64) * self.bytes_per_ms > self.queue_bytes {
            return;
        }
        self.busy_until = start + (packet.len() + 28) as f64 / self.bytes_per_ms;
        let arrive = (self.busy_until.ceil() as u64 + 15 + self.rng.below(11)).max(self.last_arrival);
        self.last_arrival = arrive;
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

/// Inputs a client sends at 30 Hz over the `congested` uplink (1 Mbit/s, 16 KiB drop-tail queue,
/// 15 + 0-10 ms; the downlink 256 kbit/s), with or without a 1 MiB bulk upload (four 256 KiB
/// messages on the bulk stream at 2 s). Returns (inputs sent, inputs that missed their tick:
/// lost, or later than 100 ms, the three ticks of input lead the harness gives `congested`),
/// and how many of the four upload messages arrived.
fn uplink_inputs(seed: u64, with_bulk: bool) -> (u64, u64, u32) {
    let mut client = Connection::new(LinkConfig::default());
    let mut server = Connection::new(LinkConfig::default());
    client.set_tick_rate(30);
    server.set_tick_rate(30);
    let mut up = Bottleneck::new(seed, 1_000_000.0, 16.0 * 1024.0);
    let mut down = Bottleneck::new(seed ^ 0x9e37, 256_000.0, 16.0 * 1024.0);
    let (mut sent, mut on_time, mut bulk_parts) = (0u64, 0u64, 0u32);
    for now in 0..40_000u64 {
        if with_bulk && now == 2_000 {
            for _ in 0..4 {
                client.send(BULK, vec![6; 256 * 1024], 0).expect("bulk queued");
            }
        }
        // One tick every 33 ms: the input, then the poll (the endpoint polls once per tick).
        if now.is_multiple_of(33) {
            if (1_000..30_000).contains(&now) {
                let mut input = vec![0xE5; 24];
                input[1..9].copy_from_slice(&now.to_le_bytes());
                client.send_essential(input, 0).expect("input queued");
                sent += 1;
            }
            for p in client.poll(now, &[]) {
                up.send(now, p);
            }
            for p in server.poll(now, &[]) {
                down.send(now, p);
            }
        }
        for p in up.due(now) {
            server.receive(&p, now).expect("valid packet");
        }
        while let Some((channel, m)) = server.next_message() {
            if channel == Channel::Unreliable && m[0] == 0xE5 {
                let mut at = [0u8; 8];
                at.copy_from_slice(&m[1..9]);
                if now - u64::from_le_bytes(at) <= 100 {
                    on_time += 1;
                }
            } else if channel == BULK {
                bulk_parts += 1;
            }
        }
        for p in down.due(now) {
            client.receive(&p, now).expect("valid packet");
        }
    }
    (sent, sent - on_time, bulk_parts)
}

/// Acceptance (spec 120 § 5.3): a 1 MiB bulk transfer plus inputs on `congested`: the inputs
/// arrive as they do without the transfer (missed inputs within +0.2 percentage points of the
/// control run), and the upload still moves (inputs do not starve it: at least half of it arrives
/// within the run; how fast it goes is the rate controller's, spec 120 § 4.2).
#[test]
fn a_1_mib_bulk_upload_leaves_input_arrival_unchanged_on_congested() {
    for seed in [3, 4, 5] {
        let (n, control_missed, _) = uplink_inputs(seed, false);
        let (m, bulk_missed, parts) = uplink_inputs(seed, true);
        assert_eq!(n, m);
        let (c, b) = (control_missed as f64 / n as f64, bulk_missed as f64 / n as f64);
        assert!(
            b <= c + 0.002,
            "seed {seed}: {:.2} % of {n} inputs missed their tick with a 1 MiB bulk upload vs {:.2} % without (bar +0.2 points)",
            100.0 * b,
            100.0 * c
        );
        assert!(
            parts >= 2,
            "seed {seed}: {parts} of 4 upload messages arrived within the run"
        );
    }
}
