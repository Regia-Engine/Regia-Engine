// SPDX-License-Identifier: MPL-2.0
//! Server input feedback, the client clock filter and the steered input lead (spec 120 § 6.4,
//! § 9.1, § 9.2; ADR-8077).
//!
//! One client and a 30 Hz server over the loopback net, 1 ms steps, seeded, virtual time. The
//! client samples its tick estimate every 16 ms frame and stamps inputs for
//! [`NetClient::due_input_ticks`]. The E3 profiles of the net-sota audit run on the crate's
//! conditioner exactly as the audit modeled them; rtt-step, clock-drift and asym-bufferbloat run
//! through [`PathSocket`], a client socket with a modeled path per direction (delay steps, jitter,
//! loss and a drop-tail bottleneck kept full by seeded cross traffic).

use std::cell::Cell;
use std::collections::VecDeque;
use std::io;
use std::net::SocketAddr;
use std::rc::Rc;

use regia_net::handshake::{ConnectRequest, Deny, PacketKind, split_datagram};
use regia_net::prelude::*;
use regia_net::wire::fnv1a_64;
use regia_net::{CryptoMode, LinkFeedback, LoopbackSocket};

struct Game;
impl NetGame for Game {
    type Message = Vec<u8>;
    type Input = i32;
    type JoinData = ();
    const GAME_ID: &'static str = "test.input-feedback";
    const PROTOCOL_VERSION: u32 = 1;
}

fn addr(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

const SERVER: u16 = 7777;
/// Inputs are judged from this long after the start, as in the audit's E3.
const WARMUP_MS: u64 = 3_000;

fn conditioner(seed: u64, delay: u64, jitter: u64, loss: f64) -> ConditionerConfig {
    ConditionerConfig {
        enabled: true,
        seed,
        loss,
        delay_ms: delay,
        jitter_ms: jitter,
        ..ConditionerConfig::default()
    }
}

/// A seeded xorshift64*.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// A drop-tail bottleneck on one direction, kept busy by seeded cross traffic.
#[derive(Debug, Clone, Copy)]
struct Bottleneck {
    /// Link rate, bytes per millisecond.
    bytes_per_ms: f64,
    /// Queue size, bytes.
    queue_bytes: f64,
    /// Cross traffic offered into the same queue, bytes per millisecond (1500-byte packets,
    /// Poisson arrivals).
    cross_bytes_per_ms: f64,
    /// When the cross traffic starts, milliseconds. 0: it is already running at the connect and
    /// the queue starts full. Later: the queue is empty until then and full from then on (a
    /// download's first burst fills it), kept full by the cross traffic.
    cross_from_ms: u64,
}

/// One direction of a modeled path.
#[derive(Debug, Clone, Default)]
struct Leg {
    delay_ms: u64,
    jitter_ms: u64,
    loss: f64,
    /// `(from ms, one-way delay ms)` changes.
    steps: Vec<(u64, u64)>,
    bottleneck: Option<Bottleneck>,
}

struct LegState {
    leg: Leg,
    rng: Rng,
    busy_until: f64,
    next_cross: f64,
    held: Vec<(u64, u64, Vec<u8>)>,
    seq: u64,
}

impl LegState {
    fn new(leg: Leg, seed: u64) -> Self {
        // The bottleneck queue starts full when the bulk download is already running; one that
        // starts later fills it at its first arrival (`cross_until`).
        let (busy_until, next_cross) = match leg.bottleneck {
            Some(b) if b.cross_from_ms == 0 => (b.queue_bytes / b.bytes_per_ms, 0.0),
            Some(b) => (0.0, b.cross_from_ms as f64),
            None => (0.0, 0.0),
        };
        Self {
            leg,
            rng: Rng(seed | 1),
            busy_until,
            next_cross,
            held: Vec::new(),
            seq: 0,
        }
    }

    fn delay(&self, now: u64) -> u64 {
        self.leg
            .steps
            .iter()
            .rev()
            .find(|(at, _)| now >= *at)
            .map_or(self.leg.delay_ms, |(_, d)| *d)
    }

    fn cross_until(&mut self, now: f64) {
        let Some(b) = self.leg.bottleneck else {
            return;
        };
        while self.next_cross <= now {
            let at = self.next_cross;
            if b.cross_from_ms > 0 && at == b.cross_from_ms as f64 {
                // The download's first burst fills the queue.
                self.busy_until = self.busy_until.max(at + b.queue_bytes / b.bytes_per_ms);
            }
            let backlog = (self.busy_until - at).max(0.0) * b.bytes_per_ms;
            if backlog + 1500.0 <= b.queue_bytes {
                self.busy_until = self.busy_until.max(at) + 1500.0 / b.bytes_per_ms;
            }
            let mean = 1500.0 / b.cross_bytes_per_ms;
            self.next_cross += -mean * (1.0 - self.rng.next()).ln();
        }
    }

    fn admit(&mut self, now: u64, bytes: Vec<u8>) {
        if self.rng.next() < self.leg.loss {
            return;
        }
        let mut depart = now as f64;
        if let Some(b) = self.leg.bottleneck {
            self.cross_until(depart);
            let backlog = (self.busy_until - depart).max(0.0) * b.bytes_per_ms;
            if backlog + bytes.len() as f64 > b.queue_bytes {
                return;
            }
            self.busy_until = self.busy_until.max(depart) + bytes.len() as f64 / b.bytes_per_ms;
            depart = self.busy_until;
        }
        let jitter = if self.leg.jitter_ms == 0 {
            0
        } else {
            (self.rng.next() * (self.leg.jitter_ms + 1) as f64) as u64
        };
        let release = depart.ceil() as u64 + self.delay(now) + jitter;
        self.seq += 1;
        self.held.push((release, self.seq, bytes));
    }

    fn due(&mut self, now: u64) -> Vec<Vec<u8>> {
        self.cross_until(now as f64);
        self.held.sort_by_key(|(at, seq, _)| (*at, *seq));
        let n = self.held.iter().take_while(|(at, _, _)| *at <= now).count();
        self.held.drain(..n).map(|(_, _, b)| b).collect()
    }
}

/// The client's socket with a modeled path to the server: `up` on what it sends, `down` on what
/// it receives.
struct PathSocket {
    inner: LoopbackSocket,
    clock: Rc<Cell<u64>>,
    server: SocketAddr,
    up: LegState,
    down: LegState,
    ready: VecDeque<Vec<u8>>,
}

impl PathSocket {
    fn pump(&mut self) -> io::Result<()> {
        let now = self.clock.get();
        for bytes in self.up.due(now) {
            self.inner.send_to(&bytes, self.server)?;
        }
        let mut buf = [0u8; 2048];
        while let Some((n, _)) = self.inner.recv_from(&mut buf)? {
            self.down.admit(now, buf[..n].to_vec());
        }
        self.ready.extend(self.down.due(now));
        Ok(())
    }
}

impl DatagramSocket for PathSocket {
    fn send_to(&mut self, bytes: &[u8], _dest: SocketAddr) -> io::Result<()> {
        let now = self.clock.get();
        self.up.admit(now, bytes.to_vec());
        // An undelayed leg delivers at once, as the plain loopback would.
        for bytes in self.up.due(now) {
            self.inner.send_to(&bytes, self.server)?;
        }
        Ok(())
    }

    fn recv_from(&mut self, buf: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
        self.pump()?;
        Ok(self.ready.pop_front().map(|b| {
            let n = b.len().min(buf.len());
            buf[..n].copy_from_slice(&b[..n]);
            (n, self.server)
        }))
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }
}

/// How a run models the network.
enum Net {
    /// The crate's conditioner on the client (up) and the server (down), as in the audit's E3.
    Conditioner(ConditionerConfig, ConditionerConfig),
    /// [`PathSocket`] legs.
    Path(Leg, Leg),
}

#[derive(Debug, Default)]
struct Outcome {
    backwards: u64,
    max_back: u64,
    missing: u64,
    ticks: u64,
    worst_streak: u64,
    late_events: u64,
    max_lead: u64,
    /// The lead at 2, 5, 10 and 20 s and at the end.
    leads: Vec<u64>,
    resyncs: u64,
    feedback_seen: bool,
    connected: bool,
}

impl Outcome {
    fn missing_pct(&self) -> f64 {
        100.0 * self.missing as f64 / self.ticks.max(1) as f64
    }
}

/// One client against a 30 Hz server for `secs`. `drift_ppm` makes the client's clock run that
/// much fast (positive) or slow against the server's.
fn run(secs: u64, net: Net, drift_ppm: i64, seed: u64) -> Outcome {
    // `INPUT_FEEDBACK_TRACE=1` prints every feedback record the client takes (diagnostics only).
    let trace = std::env::var_os("INPUT_FEEDBACK_TRACE").is_some();
    let mut last_fb = None;
    let loop_net = LoopbackNet::new();
    let clock = Rc::new(Cell::new(0u64));
    let (up_cond, down_cond, legs) = match net {
        Net::Conditioner(up, down) => (up, down, None),
        Net::Path(up, down) => (
            ConditionerConfig::default(),
            ConditionerConfig::default(),
            Some((up, down)),
        ),
    };
    let mut server: NetServer<Game, LoopbackSocket> = NetServer::with_secret(
        loop_net.bind(addr(SERVER)).expect("bind"),
        ServerConfig {
            crypto: CryptoMode::Auto,
            conditioner: down_cond,
            ..ServerConfig::default()
        },
        [7; 32],
    );
    let (up, down) = legs.unwrap_or_default();
    let socket = PathSocket {
        inner: loop_net.bind(addr(0)).expect("bind"),
        clock: clock.clone(),
        server: addr(SERVER),
        up: LegState::new(up, seed.wrapping_mul(31).wrapping_add(1)),
        down: LegState::new(down, seed.wrapping_mul(37).wrapping_add(2)),
        ready: VecDeque::new(),
    };
    let mut client: NetClient<Game, PathSocket> = NetClient::connect(
        socket,
        addr(SERVER),
        ClientConfig {
            crypto: CryptoMode::Auto,
            conditioner: up_cond,
            ..ClientConfig::default()
        },
        &(),
    )
    .expect("connect");
    let mut ticker = TickClock::new(30, 8);
    let mut out = Outcome::default();
    let mut last_est = 0u64;
    let mut streak = 0u64;
    let mut peer = None;
    for now in 0..secs * 1000 {
        clock.set(now);
        let local = (i128::from(now) * i128::from(1_000_000 + drift_ppm) / 1_000_000) as u64;
        client.receive(local);
        if trace {
            let fb = client.link_feedback().copied();
            if fb != last_fb
                && let Some(f) = fb
            {
                println!(
                    "trace t={now} server_tick={} truth={} est={} lead={} p5={} p50={} n={} contiguous={:?} late={} missing={}",
                    f.server_tick,
                    server.driver().tick(),
                    client.estimated_server_tick(local),
                    client.input_lead_ticks(),
                    f.margin_p5,
                    f.margin_p50,
                    f.margin_samples,
                    f.newest_contiguous_tick,
                    f.inputs_late,
                    f.inputs_missing
                );
            }
            last_fb = fb;
        }
        while let Some(event) = client.poll_event() {
            if let ClientEvent::InputsLate { margin_p5 } = event {
                assert!(margin_p5 < 0, "a late report carries a negative margin");
                out.late_events += 1;
            }
        }
        if client.is_connected() && now.is_multiple_of(16) {
            let est = client.estimated_server_tick(local);
            if est < last_est {
                out.backwards += 1;
                out.max_back = out.max_back.max(last_est - est);
            }
            last_est = est;
            for tick in client.due_input_ticks(local) {
                let _ = client.send_input(tick, tick as i32);
            }
            out.max_lead = out.max_lead.max(client.input_lead_ticks());
            if [2_000, 5_008, 10_000, 20_000].contains(&now) {
                out.leads.push(client.input_lead_ticks());
            }
        }
        client.flush(local);
        server.receive(now);
        while let Some(event) = server.poll_event() {
            if let ServerEvent::PeerJoined { peer: p, .. } = event {
                peer = Some(p);
            }
        }
        for _ in 0..ticker.advance(1) {
            let inputs = server.advance_tick(|_, _| {});
            if now > WARMUP_MS
                && let Some(p) = peer
            {
                out.ticks += 1;
                if inputs.contains_key(&p) {
                    streak = 0;
                } else {
                    out.missing += 1;
                    streak += 1;
                    out.worst_streak = out.worst_streak.max(streak);
                }
            }
        }
        server.flush(now);
    }
    out.leads.push(client.input_lead_ticks());
    out.connected = client.is_connected();
    out.resyncs = client.clock_resyncs();
    out.feedback_seen = client.link_feedback().is_some();
    out
}

fn e3(name: &str) -> Net {
    let (delay, jitter, loss) = match name {
        "good-wifi" => (15, 10, 0.01),
        "transatlantic" => (45, 5, 0.005),
        "bad-wifi" => (30, 60, 0.05),
        "mobile-4g" => (40, 80, 0.03),
        _ => unreachable!("{name}"),
    };
    Net::Conditioner(
        conditioner(21, delay, jitter, loss),
        conditioner(22, delay, jitter, loss),
    )
}

fn rtt_step() -> Net {
    // net-soak `rtt-step`: 20 ms one way + 0-5 ms jitter, 125 ms from 60 s, 20 ms again at 120 s.
    let leg = Leg {
        delay_ms: 20,
        jitter_ms: 5,
        steps: vec![(60_000, 125), (120_000, 20)],
        ..Leg::default()
    };
    Net::Path(leg.clone(), leg)
}

fn good_wifi_path() -> Leg {
    Leg {
        delay_ms: 15,
        jitter_ms: 10,
        loss: 0.01,
        ..Leg::default()
    }
}

fn report(name: &str, o: &Outcome) {
    println!(
        "input-feedback profile={name} connected={} est_backwards={} max_back={} resyncs={} missing={}/{} ({:.2}%) worst_streak={} late_events={} max_lead={} leads@2/5/10/20s/end={:?}",
        o.connected,
        o.backwards,
        o.max_back,
        o.resyncs,
        o.missing,
        o.ticks,
        o.missing_pct(),
        o.worst_streak,
        o.late_events,
        o.max_lead,
        o.leads
    );
}

/// Spec 120 § 9.1 bar: the tick estimate never steps backward over 10 minutes (audit E3 at the
/// base: 12 on good-wifi, 28 on bad-wifi, 29 on mobile-4g in 40 s).
#[test]
fn the_tick_estimate_never_steps_backward() {
    const TEN_MINUTES_S: u64 = 600;
    for name in ["good-wifi", "bad-wifi", "mobile-4g"] {
        let o = run(TEN_MINUTES_S, e3(name), 0, 1);
        report(name, &o);
        assert!(o.connected, "{name}");
        assert_eq!(
            o.backwards, 0,
            "{name}: the estimate stepped backward (max {} ticks)",
            o.max_back
        );
    }
    let o = run(TEN_MINUTES_S, rtt_step(), 0, 2);
    report("rtt-step", &o);
    assert!(o.connected);
    assert_eq!(
        o.backwards, 0,
        "rtt-step: the estimate stepped backward (max {} ticks)",
        o.max_back
    );
    for (name, ppm) in [("clock-drift+0.5%", 5_000), ("clock-drift-0.5%", -5_000)] {
        let o = run(
            TEN_MINUTES_S,
            Net::Path(good_wifi_path(), good_wifi_path()),
            ppm,
            3,
        );
        report(name, &o);
        assert!(o.connected, "{name}");
        assert_eq!(
            o.backwards, 0,
            "{name}: the estimate stepped backward (max {} ticks)",
            o.max_back
        );
    }
}

/// Spec 120 § 15.2 `missed_input` bars on the E3 profiles with the steered lead (audit E3 at the
/// base with the book's fixed lead of 2: 37.8 % bad-wifi, 52.6 % mobile-4g).
#[test]
fn steered_inputs_meet_the_missed_input_bars() {
    for (name, bar) in [
        ("good-wifi", 0.5),
        ("transatlantic", 0.5),
        ("bad-wifi", 2.0),
        ("mobile-4g", 2.0),
    ] {
        let o = run(40, e3(name), 0, 4);
        report(name, &o);
        assert!(o.connected && o.feedback_seen, "{name}");
        assert!(
            o.missing_pct() <= bar,
            "{name}: {:.2} % of ticks missed input (bar {bar} %)",
            o.missing_pct()
        );
    }
}

/// F2 (soak harness, high): the downstream is a 4 Mbit/s link with a 1 s drop-tail queue kept full
/// by a 4.4 Mbit/s bulk download; the uplink is good-wifi. Every tick sync arrives about a second
/// late, so the client's estimate lags and only the server can say the inputs are late. Bar:
/// missed input <= 3 % (base: about 50 %).
#[test]
fn asym_bufferbloat_inputs_arrive_in_time() {
    let o = run(40, Net::Path(good_wifi_path(), asym_down(0)), 0, 5);
    report("asym-bufferbloat", &o);
    assert!(o.connected && o.feedback_seen);
    // The download is already running at the connect, so the handshake crossed the full queue
    // and the startup lead holds half that round trip (ADR-10096): exactly what the estimate
    // lags by. No input is late and the server has nothing to report; before, the lead started
    // at two ticks, the first inputs were late and the server's report raised it. A download
    // that starts after the connect is the next test.
    assert!(
        o.late_events >= 1 || o.missing == 0,
        "the server reported the late inputs, if there were any: {} late events, {} of {} ticks missed",
        o.late_events,
        o.missing,
        o.ticks
    );
    assert!(
        o.missing_pct() <= 3.0,
        "asym-bufferbloat: {:.2} % of ticks missed input (bar 3 %)",
        o.missing_pct()
    );
    assert_eq!(o.backwards, 0);
    // The start-up spell is answered once: the late burst still in the server's window must not
    // ratchet the lead up record by record (it went 33 -> 54 when the client matched stamps by a
    // tick window instead of the p5 input's tick).
    let [at_5s, at_10s, at_20s, end] = o.leads[..] else {
        panic!("lead samples {:?}", o.leads);
    };
    assert!(
        at_10s <= at_5s && at_20s <= at_10s && end <= at_20s,
        "the lead rose after the start-up spell: {:?}",
        o.leads
    );
}

/// The same path with the download starting three seconds after the connect: the handshake and
/// the first round trips cross an empty queue, then every tick sync arrives about a second late.
/// The estimate falls behind at its 5 % slew, the client's own round trip cannot tell which
/// direction is slow, and the server's margins are what moves the lead: it follows the estimate
/// down (15 ticks in ten seconds) and no input is late on the way.
#[test]
fn a_download_that_starts_after_the_connect_is_followed_by_the_lead() {
    let o = run(40, Net::Path(good_wifi_path(), asym_down(3_000)), 0, 5);
    report("asym-bufferbloat-later", &o);
    assert!(o.connected && o.feedback_seen);
    assert!(
        o.missing_pct() <= 3.0,
        "asym-bufferbloat-later: {:.2} % of ticks missed input (bar 3 %)",
        o.missing_pct()
    );
    assert_eq!(o.backwards, 0);
    // (The samples are at 2, 5, 10 and 20 s and at the end; a client that connects after 2 s has
    // no first one.)
    let [.., at_5s, _, _, end] = o.leads[..] else {
        panic!("lead samples {:?}", o.leads);
    };
    assert!(
        end >= at_5s + 10,
        "the server's margins raised the lead as the estimate fell behind: {:?}",
        o.leads
    );
}

/// Only the server can say that inputs are late when the way to it gets slower and the way back
/// does not: the uplink goes from 15 ms to 215 ms five seconds after the connect. The tick syncs
/// still arrive on time, the round trip grows but the estimate may follow it only at its 5 %
/// slew, and the inputs arrive six ticks late until the server's report raises the lead at once.
/// (`asym_bufferbloat_inputs_arrive_in_time` was this case at the connect, before the startup
/// lead knew the handshake's round trip.) The spell is reported, it is answered once, and it
/// ends within the feedback loop: the two-second window of a record and a round trip.
#[test]
fn an_uplink_that_gets_slower_is_reported_by_the_server() {
    let up = Leg {
        steps: vec![(5_000, 215)],
        ..good_wifi_path()
    };
    let o = run(40, Net::Path(up, good_wifi_path()), 0, 5);
    report("uplink-step", &o);
    assert!(o.connected && o.feedback_seen);
    assert!(o.late_events >= 1, "the server reported the late inputs");
    assert!(
        o.missing <= 75,
        "the late spell ends within the feedback loop (2.5 s, 75 ticks): {} of {} ticks missed",
        o.missing,
        o.ticks
    );
    assert_eq!(o.backwards, 0);
    let [.., at_10s, at_20s, end] = o.leads[..] else {
        panic!("lead samples {:?}", o.leads);
    };
    assert!(
        at_20s <= at_10s && end <= at_20s,
        "the lead rose after the spell: {:?}",
        o.leads
    );
}

/// The asym-bufferbloat downstream: a 4 Mbit/s link with a 1 s drop-tail queue kept full by a
/// 4.4 Mbit/s bulk download that starts at `cross_from_ms` (0: already running at the connect).
fn asym_down(cross_from_ms: u64) -> Leg {
    Leg {
        delay_ms: 15,
        jitter_ms: 10,
        loss: 0.01,
        bottleneck: Some(Bottleneck {
            bytes_per_ms: 4_000_000.0 / 8.0 / 1000.0,
            queue_bytes: 4_000_000.0 / 8.0,
            cross_bytes_per_ms: 4_400_000.0 / 8.0 / 1000.0,
            cross_from_ms,
        }),
        ..Leg::default()
    }
}

fn connected_pair(
    sync_interval_ms: u64,
) -> (
    NetServer<Game, LoopbackSocket>,
    NetClient<Game, LoopbackSocket>,
    u64,
) {
    let net = LoopbackNet::new();
    let mut server: NetServer<Game, LoopbackSocket> = NetServer::with_secret(
        net.bind(addr(SERVER)).expect("bind"),
        ServerConfig {
            crypto: CryptoMode::Auto,
            tick_sync_interval_ms: sync_interval_ms,
            input_buffer_ticks: 40,
            ..ServerConfig::default()
        },
        [9; 32],
    );
    let mut client: NetClient<Game, LoopbackSocket> = NetClient::connect(
        net.bind(addr(0)).expect("bind"),
        addr(SERVER),
        ClientConfig::default(),
        &(),
    )
    .expect("connect");
    let mut now = 0;
    while !client.is_connected() {
        now += 1;
        assert!(now < 2_000, "connects");
        client.update(now);
        server.update(now);
    }
    (server, client, now)
}

/// A missing input is reported at once, not at the next tick sync, and the record says what the
/// server saw; `net.status` and `net.peers` carry it.
#[test]
fn a_missing_input_is_reported_before_the_next_sync() {
    let (mut server, mut client, mut now) = connected_pair(60_000);
    // Inputs for the next ticks arrive; the server simulates them.
    let start = server.driver().tick();
    for t in start..start + 3 {
        client.send_input(t, 1).expect("queued");
    }
    client.update(now + 1);
    server.update(now + 1);
    now += 1;
    for _ in 0..3 {
        server.advance_tick(|_, _| {});
    }
    server.flush(now);
    client.update(now);
    // The sync interval is a minute, so anything that arrives now is the missing-input record.
    let before = client.link_feedback().copied();
    server.advance_tick(|_, _| {});
    now += 1;
    server.flush(now);
    client.update(now);
    let fb: LinkFeedback = *client.link_feedback().expect("feedback arrived at once");
    assert_ne!(Some(fb), before);
    assert_eq!(fb.inputs_missing, 1, "{fb:?}");
    assert_eq!(fb.inputs_late, 0);
    assert_eq!(
        fb.input_buffer_ticks, 40,
        "the client learns the server's input buffer"
    );
    assert_eq!(fb.newest_contiguous_tick, Some(start + 3));
    assert!(fb.margin_samples >= 3);
    let status = client.status();
    let timing = status
        .input_timing
        .expect("connected clients report input timing");
    assert_eq!(timing.feedback, Some(fb));
    assert!(timing.lead_ticks >= 1 && u64::from(timing.lead_ticks) <= 38);
    let rows = server.peer_rows();
    assert_eq!(rows[0].input_feedback, Some(fb));
    // A late copy of an already simulated tick counts as late, and the margin goes negative.
    client.send_input(start + 4, 1).expect("queued");
    for _ in 0..10 {
        server.advance_tick(|_, _| {});
    }
    now += 1;
    client.update(now);
    server.update(now);
    now += 1;
    client.update(now);
    let fb = *client.link_feedback().expect("feedback");
    assert_eq!(fb.inputs_late, 1, "{fb:?}");
    let late: Vec<i32> = std::iter::from_fn(|| client.poll_event())
        .filter_map(|e| match e {
            ClientEvent::InputsLate { margin_p5 } => Some(margin_p5),
            _ => None,
        })
        .collect();
    assert!(fb.margin_p5 < 0 && late == vec![fb.margin_p5], "{fb:?} {late:?}");
}

/// Spectators send no inputs and get no input feedback.
#[test]
fn a_peer_without_inputs_gets_no_feedback() {
    let (mut server, mut client, mut now) = connected_pair(50);
    for _ in 0..200 {
        now += 5;
        server.advance_tick(|_, _| {});
        server.update(now);
        client.update(now);
    }
    assert!(client.link_feedback().is_none());
    assert!(client.estimated_server_tick(now) > 0, "tick syncs still arrive");
}

/// A peer built before the input feedback (wire version 3) is refused at the handshake with
/// `protocol_mismatch`: it would read the feedback lane as an unknown lane.
#[test]
fn a_wire_version_3_peer_is_refused_cleanly() {
    // Wire 5 (transport v2, ADR-8184) and wire 6 (the one-round-trip resume, ADR-8858) moved the
    // current version on; a wire-3 peer is still refused.
    assert_eq!(regia_net::REGIA_NET_WIRE_VERSION, 6);
    let net = LoopbackNet::new();
    let mut server: NetServer<Game, LoopbackSocket> = NetServer::with_secret(
        net.bind(addr(SERVER)).expect("bind"),
        ServerConfig {
            crypto: CryptoMode::Required,
            ..ServerConfig::default()
        },
        [3; 32],
    );
    // The protocol id a wire-version-3 build of the same game computes (`protocol_id`).
    let mut w = WireWriter::new();
    w.u32(3);
    w.raw(Game::GAME_ID.as_bytes());
    w.u8(0);
    w.u32(Game::PROTOCOL_VERSION);
    let old_id = fnv1a_64(w.as_slice());
    assert_ne!(old_id, regia_net::protocol_id::<Game>());
    let mut old = net.bind(addr(4242)).expect("bind old client");
    let request = ConnectRequest {
        protocol_id: old_id,
        client_nonce: 0xB0B,
        crypto_version: 2,
        crypto_required: true,
    };
    old.send_to(&request.to_datagram(), addr(SERVER)).expect("send");
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
    assert_eq!(deny.client_nonce, 0xB0B);
    assert_eq!(deny.reason.as_str(), "protocol_mismatch");
    assert_eq!(server.session().len(), 0, "nobody joined");
}
