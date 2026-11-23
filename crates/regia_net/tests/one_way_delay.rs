// SPDX-License-Identifier: MPL-2.0
//! ADR-10050: the rate controller's queue readings are of the sender's own packets on the way
//! out. Two connections over virtual-time links with a serializing bottleneck in one direction
//! (a FIFO at a fixed bit rate, as a slow access link is); no wall clock, no randomness.
use std::collections::VecDeque;

use regia_net::connection::{Channel, Connection, LinkConfig};

/// UDP and IP header bytes a datagram costs on the link.
const HEADER_BYTES: usize = 28;
/// Both endpoints read their sockets and send once per tick, half a tick apart.
const TICK_MS: u64 = 16;

/// One direction of a path: an optional bottleneck that serializes one datagram at a time, then
/// a fixed propagation delay.
struct Link {
    /// Bytes per millisecond of the bottleneck (`None`: no bottleneck).
    bytes_per_ms: Option<f64>,
    delay_ms: f64,
    /// Extra delay of each datagram, uniform in `0..=jitter_ms` from a seeded generator. Order
    /// is kept: a datagram never arrives before the one sent ahead of it.
    jitter_ms: u64,
    rng: u64,
    last_arrival_ms: f64,
    /// When the bottleneck finishes the datagram it is sending.
    free_at_ms: f64,
    /// `(arrival, datagram)`, in order.
    flying: VecDeque<(f64, Vec<u8>)>,
    /// The longest a datagram waited for the bottleneck, milliseconds.
    worst_wait_ms: f64,
}

impl Link {
    fn new(bit_rate: Option<u32>, delay_ms: f64) -> Self {
        Self {
            bytes_per_ms: bit_rate.map(|b| f64::from(b) / 8_000.0),
            delay_ms,
            jitter_ms: 0,
            rng: 0,
            last_arrival_ms: 0.0,
            free_at_ms: 0.0,
            flying: VecDeque::new(),
            worst_wait_ms: 0.0,
        }
    }

    fn with_jitter(mut self, jitter_ms: u64, seed: u64) -> Self {
        self.jitter_ms = jitter_ms;
        self.rng = seed | 1;
        self
    }

    fn send(&mut self, now_ms: u64, datagram: Vec<u8>) {
        let now = now_ms as f64;
        let left = match self.bytes_per_ms {
            Some(rate) => {
                let start = now.max(self.free_at_ms);
                self.worst_wait_ms = self.worst_wait_ms.max(start - now);
                self.free_at_ms = start + (datagram.len() + HEADER_BYTES) as f64 / rate;
                self.free_at_ms
            }
            None => now,
        };
        let mut jitter = 0.0;
        if self.jitter_ms > 0 {
            // xorshift64 (Marsaglia 2003): any seeded generator would do.
            self.rng ^= self.rng << 13;
            self.rng ^= self.rng >> 7;
            self.rng ^= self.rng << 17;
            jitter = (self.rng % (self.jitter_ms + 1)) as f64;
        }
        self.last_arrival_ms = self.last_arrival_ms.max(left + self.delay_ms + jitter);
        self.flying.push_back((self.last_arrival_ms, datagram));
    }

    fn due(&mut self, now_ms: u64) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        while self.flying.front().is_some_and(|(at, _)| *at <= now_ms as f64) {
            if let Some((_, d)) = self.flying.pop_front() {
                out.push(d);
            }
        }
        out
    }
}

struct Path {
    client: Connection,
    server: Connection,
    up: Link,
    down: Link,
}

impl Path {
    /// The client's controller runs; the server's is off, so the server's traffic is exactly
    /// what the test queues.
    fn new(up: Link, down: Link) -> Self {
        let mut server = LinkConfig::default();
        server.rate.enabled = false;
        Self {
            client: Connection::new(LinkConfig::default()),
            server: Connection::new(server),
            up,
            down,
        }
    }

    /// One client tick at `now_ms` and one server tick half a tick later.
    fn tick(&mut self, now_ms: u64, client_sends: &[(Channel, usize)], server_sends: &[(Channel, usize)]) {
        self.client_tick(now_ms, client_sends);
        self.server_tick(now_ms + TICK_MS / 2, server_sends);
    }

    /// The client reads its socket, queues `sends` and sends.
    fn client_tick(&mut self, now_ms: u64, client_sends: &[(Channel, usize)]) {
        for d in self.down.due(now_ms) {
            self.client.receive(&d, now_ms).expect("server packet");
        }
        while self.client.next_message().is_some() {}
        for (channel, len) in client_sends {
            // A full send buffer refuses the message: the application offers it again next tick.
            let _ = self.client.send(*channel, vec![7; *len], 0);
        }
        for d in self.client.poll(now_ms, &[]) {
            self.up.send(now_ms, d);
        }
    }

    /// The server reads its socket, queues `sends` and sends.
    fn server_tick(&mut self, now_ms: u64, server_sends: &[(Channel, usize)]) {
        for d in self.up.due(now_ms) {
            self.server.receive(&d, now_ms).expect("client packet");
        }
        while self.server.next_message().is_some() {}
        for (channel, len) in server_sends {
            self.server.send(*channel, vec![9; *len], 0).expect("queued");
        }
        for d in self.server.poll(now_ms, &[]) {
            self.down.send(now_ms, d);
        }
    }
}

/// A guard for ADR-10050 (it passes before and after): a queue the sender builds on its own way
/// out is still read and still cut. The client uploads over a
/// 256 kbit/s uplink (a reliable stream the application keeps full) while the way back is fast.
/// The controller must find the bottleneck (cuts) and hold the queue there.
#[test]
fn a_queue_on_the_way_out_still_cuts() {
    let mut path = Path::new(Link::new(Some(256_000), 20.0), Link::new(None, 20.0));
    let mut now = 0;
    let mut late_wait_ms = 0.0f64;
    while now < 30_000 {
        // 4,000 bytes a tick is 2 Mbit/s offered: eight times the link.
        path.tick(now, &[(Channel::GAME_RPC, 4_000)], &[(Channel::Unreliable, 60)]);
        if now == 10_000 {
            path.up.worst_wait_ms = 0.0;
        }
        if now > 10_000 {
            late_wait_ms = late_wait_ms.max(path.up.worst_wait_ms);
        }
        now += TICK_MS;
    }
    let rate = path.client.rate_controller();
    assert!(rate.cuts() > 0, "a sender over its link's rate was never cut");
    assert!(
        rate.target_bytes_per_second() <= 40_000,
        "target {} B/s on a 32,000 B/s link",
        rate.target_bytes_per_second()
    );
    assert!(
        late_wait_ms <= 400.0,
        "after 10 s the sender's packets still wait {late_wait_ms:.0} ms at its own bottleneck"
    );
}

/// The client of the congested soak rows, as one path (ADR-10050). Its way out has no
/// bottleneck; the way back is a 256 kbit/s link that the 30 Hz server's snapshots keep 70 % busy,
/// with a burst once a second and, now and then, a tick with no snapshot (the acknowledgment then
/// rides alone in a small packet, 20 ms sooner than one behind a snapshot: the minimum round
/// trip). Both directions jitter by 10 ms, and each side reads its socket once per tick, so
/// every round trip also carries up to a tick of each read's phase. The client sends one small
/// input a tick: its rounds hold one to three delay samples and it uses a fraction of its
/// target. Read from round trips alone, that is a queue over the threshold that keeps "rising":
/// the client cut its unused target again and again (325 times in these eight seeded minutes).
/// Its own packets' one-way delay shows nothing standing on the way out, so a verdict waits for
/// six one-way samples in a row to sit 12 ms over the minimum: 233 cuts are left, the ones whose
/// samples' read phase and jitter did that.
#[test]
fn a_thin_sender_behind_a_busy_return_path_is_cut_less() {
    let mut cuts = Vec::new();
    for seed in 1..=8u64 {
        let mut path = Path::new(
            Link::new(None, 15.0).with_jitter(10, seed),
            Link::new(Some(256_000), 15.0).with_jitter(10, seed + 100),
        );
        let mut server_tick = 0u64;
        for now in 0..60_000u64 {
            if now.is_multiple_of(TICK_MS) {
                path.client_tick(now, &[(Channel::Unreliable, 80)]);
            }
            if now % 33 == 5 {
                let mut sends = Vec::new();
                if !server_tick.is_multiple_of(20) {
                    sends.push((Channel::Unreliable, 700));
                }
                if server_tick % 30 == 7 {
                    sends.push((Channel::Unreliable, 1_000));
                    sends.push((Channel::Unreliable, 1_000));
                }
                path.server_tick(now, &sends);
                server_tick += 1;
            }
        }
        assert_eq!(
            path.up.worst_wait_ms, 0.0,
            "the scenario: nothing waits on the way out"
        );
        cuts.push(path.client.rate_controller().cuts());
    }
    let total: u64 = cuts.iter().sum();
    eprintln!("client cuts per seed {cuts:?}, total {total}");
    assert!(
        total <= THIN_SENDER_CUTS,
        "the client cut its rate {total} times in eight minutes with nothing queued on its way out \
         (per seed {cuts:?}; the bound is {THIN_SENDER_CUTS})"
    );
}

/// The bound of `a_thin_sender_behind_a_busy_return_path_is_cut_less`: between the 233 cuts the
/// rule leaves and the 325 of round trips alone (both measured on this scenario; the runs are
/// seeded and in virtual time).
const THIN_SENDER_CUTS: u64 = 250;
