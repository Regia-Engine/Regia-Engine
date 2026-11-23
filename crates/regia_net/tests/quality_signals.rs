// SPDX-License-Identifier: MPL-2.0
//! Connection quality signals (spec 120 § 13.1, ADR-8182): the per-peer `PeerQuality`, its
//! `LinkGrade` with hysteresis and the `QualityChanged` events, two-way loss, one-way jitter and
//! the socket drop counter.
//!
//! One client and a 30 Hz server over the loopback net, 1 ms steps, seeded, virtual time. The
//! client's socket models each direction of the path with the net-soak harness's profile numbers
//! (good-wifi, bad-wifi with its Gilbert-Elliott loss, flappy) and the net-sota audit's
//! downstream-loss profile. The client stamps an input every 16 ms frame.

use std::cell::Cell;
use std::collections::VecDeque;
use std::io;
use std::net::SocketAddr;
use std::rc::Rc;

use regia_net::prelude::*;
use regia_net::{CryptoMode, LinkGrade, LoopbackSocket, PeerQuality};

struct Game;
impl NetGame for Game {
    type Message = Vec<u8>;
    type Input = i32;
    type JoinData = ();
    const GAME_ID: &'static str = "test.quality-signals";
    const PROTOCOL_VERSION: u32 = 1;
}

fn addr(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

const SERVER: u16 = 7777;

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

/// Gilbert-Elliott burst loss: the chance to enter the bad state per packet, to leave it, and
/// the loss inside it.
#[derive(Debug, Clone, Copy)]
struct Burst {
    to_bad: f64,
    to_good: f64,
    bad_loss: f64,
}

/// One direction of a modeled path.
#[derive(Debug, Clone, Default)]
struct Leg {
    delay_ms: u64,
    jitter_ms: u64,
    loss: f64,
    burst: Option<Burst>,
    reorder: f64,
    reorder_ms: u64,
    duplicate: f64,
}

impl Leg {
    /// net-soak `good-wifi`: 4 + 0-4 ms, 0.2 % loss, 0.1 % reorders (10 ms), 0.05 % duplicates.
    fn good_wifi() -> Self {
        Self {
            delay_ms: 4,
            jitter_ms: 4,
            loss: 0.002,
            reorder: 0.001,
            reorder_ms: 10,
            duplicate: 0.0005,
            ..Self::default()
        }
    }

    /// net-soak `bad-wifi`: 8 + 0-30 ms, 1 % loss plus Gilbert-Elliott bursts (0.01 to bad, 0.3
    /// back, 60 % lost in bad; about 3 % in total), 1 % reorders (30 ms), 0.5 % duplicates.
    fn bad_wifi() -> Self {
        Self {
            delay_ms: 8,
            jitter_ms: 30,
            loss: 0.01,
            burst: Some(Burst {
                to_bad: 0.01,
                to_good: 0.3,
                bad_loss: 0.6,
            }),
            reorder: 0.01,
            reorder_ms: 30,
            duplicate: 0.005,
        }
    }

    /// net-sota audit `downstream-loss`: 30 ms one way, the given loss.
    fn fixed(delay_ms: u64, loss: f64) -> Self {
        Self {
            delay_ms,
            loss,
            ..Self::default()
        }
    }
}

struct LegState {
    leg: Leg,
    rng: Rng,
    bad: bool,
    held: Vec<(u64, u64, Vec<u8>)>,
    seq: u64,
    /// `(admitted, dropped)`, shared with the test.
    counts: Rc<Cell<(u64, u64)>>,
}

impl LegState {
    fn new(leg: Leg, seed: u64) -> Self {
        Self {
            leg,
            rng: Rng(seed | 1),
            bad: false,
            held: Vec::new(),
            seq: 0,
            counts: Rc::new(Cell::new((0, 0))),
        }
    }

    fn admit(&mut self, now: u64, bytes: Vec<u8>, outage: bool) {
        let (admitted, dropped) = self.counts.get();
        self.counts.set((admitted + 1, dropped));
        let mut loss = self.leg.loss;
        if let Some(b) = self.leg.burst {
            let flip = self.rng.next();
            self.bad = if self.bad {
                flip >= b.to_good
            } else {
                flip < b.to_bad
            };
            if self.bad {
                loss = 1.0 - (1.0 - loss) * (1.0 - b.bad_loss);
            }
        }
        if outage || self.rng.next() < loss {
            self.counts.set((admitted + 1, dropped + 1));
            return;
        }
        let copies = if self.rng.next() < self.leg.duplicate {
            2
        } else {
            1
        };
        for _ in 0..copies {
            let jitter = if self.leg.jitter_ms == 0 {
                0
            } else {
                (self.rng.next() * (self.leg.jitter_ms + 1) as f64) as u64
            };
            let reorder = if self.rng.next() < self.leg.reorder {
                (self.rng.next() * (self.leg.reorder_ms + 1) as f64) as u64
            } else {
                0
            };
            self.seq += 1;
            self.held.push((
                now + self.leg.delay_ms + jitter + reorder,
                self.seq,
                bytes.clone(),
            ));
        }
    }

    fn due(&mut self, now: u64) -> Vec<Vec<u8>> {
        self.held.sort_by_key(|(at, seq, _)| (*at, *seq));
        let n = self.held.iter().take_while(|(at, _, _)| *at <= now).count();
        self.held.drain(..n).map(|(_, _, b)| b).collect()
    }
}

/// The client's socket with a modeled path: `up` on what it sends, `down` on what it receives;
/// `outage` drops both directions.
struct PathSocket {
    inner: LoopbackSocket,
    clock: Rc<Cell<u64>>,
    outage: Rc<Cell<bool>>,
    server: SocketAddr,
    up: LegState,
    down: LegState,
    ready: VecDeque<Vec<u8>>,
    /// Every other datagram is absorbed as a full send buffer would (the drop counter test).
    drop_every_other: Rc<Cell<bool>>,
    /// `(send_to calls, absorbed)`, shared with the test.
    sends: Rc<Cell<(u64, u64)>>,
}

impl PathSocket {
    fn pump(&mut self) -> io::Result<()> {
        let now = self.clock.get();
        for bytes in self.up.due(now) {
            self.inner.send_to(&bytes, self.server)?;
        }
        let mut buf = [0u8; 2048];
        while let Some((n, _)) = self.inner.recv_from(&mut buf)? {
            self.down.admit(now, buf[..n].to_vec(), self.outage.get());
        }
        self.ready.extend(self.down.due(now));
        Ok(())
    }
}

impl DatagramSocket for PathSocket {
    fn send_to(&mut self, bytes: &[u8], _dest: SocketAddr) -> io::Result<()> {
        let (sends, absorbed) = self.sends.get();
        if self.drop_every_other.get() && (sends + 1).is_multiple_of(2) {
            self.sends.set((sends + 1, absorbed + 1));
            // The datagram did not leave: the base's contract (ADR-8407) is an error, as for a
            // full send buffer.
            return Err(io::ErrorKind::WouldBlock.into());
        }
        self.sends.set((sends + 1, absorbed));
        let now = self.clock.get();
        self.up.admit(now, bytes.to_vec(), self.outage.get());
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

/// A grade change and when it happened, milliseconds.
type Changes = Vec<(u64, LinkGrade, LinkGrade)>;

struct Run {
    server: NetServer<Game, LoopbackSocket>,
    client: NetClient<Game, PathSocket>,
    clock: Rc<Cell<u64>>,
    outage: Rc<Cell<bool>>,
    drop_every_other: Rc<Cell<bool>>,
    sends: Rc<Cell<(u64, u64)>>,
    up_counts: Rc<Cell<(u64, u64)>>,
    ticker: TickClock,
    now: u64,
    peer: Option<PeerId>,
    connected_at: Option<u64>,
    client_changes: Changes,
    server_changes: Changes,
    interrupted: u64,
    restored: u64,
}

impl Run {
    fn new(up: Leg, down: Leg, seed: u64) -> Self {
        let net = LoopbackNet::new();
        let clock = Rc::new(Cell::new(0u64));
        let outage = Rc::new(Cell::new(false));
        let drop_every_other = Rc::new(Cell::new(false));
        let sends = Rc::new(Cell::new((0, 0)));
        let up = LegState::new(up, seed.wrapping_mul(31).wrapping_add(1));
        let up_counts = up.counts.clone();
        let server = NetServer::with_secret(
            net.bind(addr(SERVER)).expect("bind"),
            ServerConfig {
                crypto: CryptoMode::Off,
                ..ServerConfig::default()
            },
            [7; 32],
        );
        let socket = PathSocket {
            inner: net.bind(addr(0)).expect("bind"),
            clock: clock.clone(),
            outage: outage.clone(),
            server: addr(SERVER),
            up,
            down: LegState::new(down, seed.wrapping_mul(37).wrapping_add(2)),
            ready: VecDeque::new(),
            drop_every_other: drop_every_other.clone(),
            sends: sends.clone(),
        };
        let client = NetClient::connect(
            socket,
            addr(SERVER),
            ClientConfig {
                crypto: CryptoMode::Off,
                ..ClientConfig::default()
            },
            &(),
        )
        .expect("connect");
        Self {
            server,
            client,
            clock,
            outage,
            drop_every_other,
            sends,
            up_counts,
            ticker: TickClock::new(30, 8),
            now: 0,
            peer: None,
            connected_at: None,
            client_changes: Vec::new(),
            server_changes: Vec::new(),
            interrupted: 0,
            restored: 0,
        }
    }

    /// One millisecond.
    fn step(&mut self) {
        let now = self.now;
        self.clock.set(now);
        self.client.receive(now);
        while let Some(event) = self.client.poll_event() {
            match event {
                ClientEvent::Connected { .. } => self.connected_at = Some(now),
                ClientEvent::QualityChanged { from, to } => self.client_changes.push((now, from, to)),
                ClientEvent::PeerInterrupted => self.interrupted += 1,
                ClientEvent::PeerRestored => self.restored += 1,
                _ => {}
            }
        }
        if self.client.is_connected() && now.is_multiple_of(16) {
            for tick in self.client.due_input_ticks(now) {
                let _ = self.client.send_input(tick, tick as i32);
            }
        }
        self.client.flush(now);
        self.server.receive(now);
        while let Some(event) = self.server.poll_event() {
            match event {
                ServerEvent::PeerJoined { peer, .. } => self.peer = Some(peer),
                ServerEvent::QualityChanged { peer, from, to } => {
                    assert_eq!(Some(peer), self.peer);
                    self.server_changes.push((now, from, to));
                }
                _ => {}
            }
        }
        for _ in 0..self.ticker.advance(1) {
            self.server.advance_tick(|_, _| {});
        }
        self.server.flush(now);
        self.now += 1;
    }

    fn run_ms(&mut self, ms: u64) {
        for _ in 0..ms {
            self.step();
        }
    }

    fn client_quality(&self) -> PeerQuality {
        self.client.quality().expect("connected")
    }

    fn server_quality(&self) -> PeerQuality {
        self.server
            .peer_quality(self.peer.expect("joined"))
            .expect("peer quality")
    }
}

fn changes_to(changes: &Changes, to: LinkGrade) -> usize {
    changes.iter().filter(|(_, _, t)| *t == to).count()
}

/// Q1 acceptance: under good-wifi the grade stays good for 10 minutes, on both ends, and no
/// grade change is ever raised, for eight seeds.
#[test]
fn good_wifi_grade_stays_good_for_ten_minutes() {
    for seed in 1..=8u64 {
        good_wifi_ten_minutes(seed);
    }
}

fn good_wifi_ten_minutes(seed: u64) {
    let mut r = Run::new(Leg::good_wifi(), Leg::good_wifi(), seed);
    r.run_ms(600_000);
    assert!(r.client.is_connected());
    let (c, s) = (r.client_quality(), r.server_quality());
    assert_eq!(
        r.client_changes,
        Vec::new(),
        "seed {seed}: client grade changes; last quality {c:?}"
    );
    assert_eq!(
        r.server_changes,
        Vec::new(),
        "seed {seed}: server grade changes; last quality {s:?}"
    );
    assert_eq!((c.grade, s.grade), (LinkGrade::Good, LinkGrade::Good));
    // The signals are live, not zero: an RTT, a small jitter and loss far under the limit.
    assert!(c.rtt_p95_ms > 8.0 && c.rtt_p95_ms < 40.0, "{c:?}");
    assert!(c.jitter_ms > 0.0 && c.jitter_ms < 4.0, "{c:?}");
    assert!(c.loss() < 0.01 && s.loss() < 0.01, "{c:?} {s:?}");
    assert!(s.inputs_missing < 0.005, "{s:?}");
}

/// Q1 acceptance: bad-wifi reports fair or poor within 3 s of the connection, on both ends, for
/// every seed.
///
/// Owner-ruled known issue for v0.7.0 (KI-NT-053, ADR-10373): on wire 5 the first `Fair`/`Poor`
/// lands 2.5-6.5 s after the connect, against this unchanged 3 s bar. The bar and the assertions
/// stay as they are; the ignore goes with the v0.8 fix (faster confidence-bound grading).
#[test]
#[ignore = "owner-ruled known issue for v0.7.0 (KI-NT-053, ADR-10373): first Fair/Poor 2.5-6.5 s on wire 5 against the unchanged 3 s bar; fixed in v0.8 by faster confidence-bound grading; remove the ignore with the fix"]
fn bad_wifi_reports_fair_or_poor_within_three_seconds() {
    for seed in 1..=8u64 {
        let mut r = Run::new(Leg::bad_wifi(), Leg::bad_wifi(), seed);
        r.run_ms(20_000);
        let connected = r.connected_at.expect("connected");
        let first_bad = |changes: &Changes| {
            changes
                .iter()
                .find(|(_, _, to)| matches!(to, LinkGrade::Fair | LinkGrade::Poor))
                .map(|(at, _, _)| at - connected)
        };
        let client = first_bad(&r.client_changes);
        let server = first_bad(&r.server_changes);
        assert!(
            client.is_some_and(|ms| ms <= 3_000),
            "seed {seed}: client first fair/poor after {client:?} ms; {:?}",
            r.client_quality()
        );
        assert!(
            server.is_some_and(|ms| ms <= 3_000),
            "seed {seed}: server first fair/poor after {server:?} ms; {:?}",
            r.server_quality()
        );
    }
}

/// Q1 acceptance: the client sees the upstream loss the server measured, within 2 %, and both
/// are near the loss the path injected.
#[test]
fn the_client_sees_the_upstream_loss_the_server_measured() {
    for seed in 1..=4u64 {
        let mut r = Run::new(Leg::bad_wifi(), Leg::bad_wifi(), seed);
        // Check every second from 5 s on: a report is at most one tick sync old.
        r.run_ms(5_000);
        for _ in 0..25 {
            r.run_ms(1_000);
            let (c, s) = (r.client_quality(), r.server_quality());
            assert!(
                (c.loss_in - s.loss_recv).abs() <= 0.02,
                "seed {seed} t={}: client loss_in {} vs server-measured {}",
                r.now,
                c.loss_in,
                s.loss_recv
            );
        }
        let (admitted, dropped) = r.up_counts.get();
        let injected = dropped as f32 / admitted as f32;
        let s = r.server_quality();
        assert!(s.loss_recv > 0.0, "seed {seed}: the server measured loss");
        assert!(
            (s.loss_recv - injected).abs() <= 0.03,
            "seed {seed}: measured {} vs injected {injected} over the run",
            s.loss_recv
        );
    }
}

/// Q1 acceptance: flappy (3 s outages every 20 s from 15 s) shows `Interrupted` exactly once per
/// outage on both ends, and the grade comes back after each.
#[test]
fn flappy_shows_interrupted_once_per_outage() {
    let mut r = Run::new(Leg::good_wifi(), Leg::good_wifi(), 3);
    for _ in 0..120_000 {
        let t = r.now;
        r.outage.set(t >= 15_000 && (t - 15_000) % 20_000 < 3_000);
        r.step();
    }
    assert!(r.client.is_connected());
    assert_eq!(r.interrupted, 6, "six outages in 120 s");
    for (side, changes) in [("client", &r.client_changes), ("server", &r.server_changes)] {
        assert_eq!(
            changes_to(changes, LinkGrade::Interrupted),
            6,
            "{side}: {changes:?}"
        );
        // The silence before `Interrupted` and the restore move no other grade.
        assert_eq!(
            changes.len(),
            12,
            "{side}: only interrupted and back: {changes:?}"
        );
        let restored = changes
            .iter()
            .filter(|(_, from, _)| *from == LinkGrade::Interrupted)
            .count();
        assert_eq!(restored, 6, "{side}: one restore per outage: {changes:?}");
        // Every interruption falls inside an outage (plus the interrupt interval).
        for (at, _, to) in changes.iter().filter(|(_, _, to)| *to == LinkGrade::Interrupted) {
            let into = (at - 15_000) % 20_000;
            assert!(
                *to == LinkGrade::Interrupted && into < 3_000 + 100,
                "{side}: at {at}"
            );
        }
    }
    assert_eq!(
        r.client_quality().grade,
        LinkGrade::Good,
        "{:?} {:?}",
        r.client_quality(),
        r.client_changes
    );
    assert_eq!(
        r.server_quality().grade,
        LinkGrade::Good,
        "{:?} {:?}",
        r.server_quality(),
        r.server_changes
    );
}

/// Q1 acceptance: with loss only on the downstream (net-sota `downstream-loss`: 0 % up, 15 %
/// down, 30 ms), the client reports it on the downstream (`loss_recv`) and the server on what it
/// sent (`loss_out`); the upstream reads clean on both ends.
#[test]
fn downstream_loss_is_reported_on_the_right_direction() {
    let mut r = Run::new(Leg::fixed(30, 0.0), Leg::fixed(30, 0.15), 5);
    r.run_ms(30_000);
    let (c, s) = (r.client_quality(), r.server_quality());
    assert!((c.loss_recv - 0.15).abs() <= 0.03, "client downstream {c:?}");
    assert!(c.loss_in < 0.005, "client upstream as the server saw it {c:?}");
    assert!(c.loss_out < 0.02, "client upstream from acknowledgments {c:?}");
    assert!(s.loss_recv < 0.005, "server upstream {s:?}");
    assert!((s.loss_out - 0.15).abs() <= 0.03, "server downstream {s:?}");
    assert_eq!((c.grade, s.grade), (LinkGrade::Poor, LinkGrade::Poor));
    // And the mirror image: upstream loss only.
    let mut r = Run::new(Leg::fixed(30, 0.15), Leg::fixed(30, 0.0), 6);
    r.run_ms(30_000);
    let (c, s) = (r.client_quality(), r.server_quality());
    assert!(c.loss_recv < 0.005, "client downstream {c:?}");
    assert!(
        (c.loss_in - 0.15).abs() <= 0.03,
        "client upstream as the server saw it {c:?}"
    );
    assert!((s.loss_recv - 0.15).abs() <= 0.03, "server upstream {s:?}");
    assert!(s.loss_out < 0.02, "server downstream {s:?}");
}

/// The one-way jitter is the RFC 3550 interarrival jitter of the peer's clock stamps: about zero
/// on a constant-delay path, larger on bad-wifi than on good-wifi, and measured on both ends.
#[test]
fn one_way_jitter_tracks_the_path_on_both_ends() {
    let mut flat = Run::new(Leg::fixed(40, 0.0), Leg::fixed(40, 0.0), 7);
    flat.run_ms(20_000);
    let (c, s) = (flat.client_quality(), flat.server_quality());
    assert!(c.jitter_ms < 1.0 && s.jitter_ms < 1.0, "{c:?} {s:?}");
    assert!(c.rttvar_ms >= 0.0 && c.rtt_p50_ms >= 80.0, "{c:?}");
    let mut good = Run::new(Leg::good_wifi(), Leg::good_wifi(), 7);
    good.run_ms(20_000);
    let mut bad = Run::new(Leg::bad_wifi(), Leg::bad_wifi(), 7);
    bad.run_ms(20_000);
    let (gc, gs, bc, bs) = (
        good.client_quality(),
        good.server_quality(),
        bad.client_quality(),
        bad.server_quality(),
    );
    // Uniform 0-4 ms: E|D| = 1.6 ms; uniform 0-30 ms plus reorders: about 10 ms.
    assert!(gc.jitter_ms > 0.3 && gc.jitter_ms < 4.0, "{gc:?}");
    assert!(gs.jitter_ms > 0.3 && gs.jitter_ms < 4.0, "{gs:?}");
    assert!(bc.jitter_ms > 5.0 && bc.jitter_ms < 20.0, "{bc:?}");
    assert!(bs.jitter_ms > 5.0 && bs.jitter_ms < 20.0, "{bs:?}");
}

/// The rate controller's view of the path reaches `PeerQuality` (spec 120 § 4.2 and § 13.1,
/// ADR-8889): the allowance the game may use and the queueing delay over the minimum RTT, on both
/// ends; bad-wifi's 0-30 ms jitter shows as queueing delay in some rounds.
#[test]
fn the_rate_controller_signals_reach_the_quality() {
    let mut r = Run::new(Leg::bad_wifi(), Leg::bad_wifi(), 10);
    r.run_ms(2_000);
    let (mut queue_c, mut queue_s) = (0.0f32, 0.0f32);
    for _ in 0..20 {
        r.run_ms(500);
        let (c, s) = (r.client_quality(), r.server_quality());
        queue_c = queue_c.max(c.queue_delay_ms);
        queue_s = queue_s.max(s.queue_delay_ms);
        assert!(c.send_allowance_bps > 0, "{c:?}");
        assert!(s.send_allowance_bps > 0, "{s:?}");
    }
    assert!(
        queue_c > 0.0 && queue_s > 0.0,
        "client {queue_c} ms, server {queue_s} ms"
    );
}

/// Datagrams the socket absorbs are counted as dropped, not as sent (ADR-8182; the counter is the
/// base's `socket_drops`, ADR-8407, and an absorbed send is an error there, ADR-8889).
#[test]
fn socket_drops_are_not_counted_as_sent() {
    let mut r = Run::new(Leg::fixed(5, 0.0), Leg::fixed(5, 0.0), 8);
    r.drop_every_other.set(true);
    r.run_ms(5_000);
    let totals = r.client.totals();
    let (sends, absorbed) = r.sends.get();
    assert!(totals.socket_drops > 0);
    assert_eq!(totals.socket_drops, absorbed);
    assert_eq!(totals.datagrams_out + totals.socket_drops, sends);
}

/// `net.peers` rows and `net.status` carry the grade and the quality (ADR-8182).
#[test]
fn peer_rows_and_status_carry_the_grade() {
    let mut r = Run::new(Leg::fixed(30, 0.15), Leg::fixed(30, 0.15), 9);
    r.run_ms(15_000);
    let rows = r.server.peer_rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].grade, LinkGrade::Poor);
    assert_eq!(rows[0].quality.map(|q| q.grade), Some(LinkGrade::Poor));
    let status = r.client.status();
    assert_eq!(status.quality.map(|q| q.grade), Some(LinkGrade::Poor));
    let overlay = r.server.overlay();
    assert_eq!(overlay.worst_grade, LinkGrade::Poor);
    assert!(overlay.lines()[1].contains(" poor rtt"), "{:?}", overlay.lines());
}

/// Diagnostics only: the client's signals every 250 ms over the first 4 s of bad-wifi.
#[test]
#[ignore = "diagnostics"]
fn diag_bad_wifi_start() {
    for seed in 1..=8u64 {
        let mut r = Run::new(Leg::bad_wifi(), Leg::bad_wifi(), seed);
        while r.connected_at.is_none() {
            r.step();
        }
        let c0 = r.connected_at.unwrap_or(0);
        for _ in 0..16 {
            r.run_ms(250);
            let (c, s) = (r.client_quality(), r.server_quality());
            println!(
                "seed {seed} t+{} c out {:.3} in {:.3} recv {:.3} grade {:?} | s out {:.3} recv {:.3} grade {:?}",
                r.now - c0,
                c.loss_out,
                c.loss_in,
                c.loss_recv,
                c.grade,
                s.loss_out,
                s.loss_recv,
                s.grade
            );
        }
        println!(
            "seed {seed} changes c {:?} s {:?}",
            r.client_changes, r.server_changes
        );
    }
}

/// Prints every quality sample of the first 6 s (good-wifi seed 3, bad-wifi seed 1).
#[test]
#[ignore = "diagnostics"]
fn diag_first_seconds() {
    for (name, leg, seed) in [("good", Leg::good_wifi(), 3u64), ("bad", Leg::bad_wifi(), 1)] {
        let mut r = Run::new(leg.clone(), leg, seed);
        while r.connected_at.is_none() {
            r.step();
        }
        let c0 = r.connected_at.unwrap_or(0);
        for _ in 0..24 {
            r.run_ms(250);
            println!("{name} t+{} c {:?}", r.now - c0, r.client_quality());
            println!("{name} t+{} s {:?}", r.now - c0, r.server_quality());
        }
        println!("{name} changes c {:?} s {:?}", r.client_changes, r.server_changes);
    }
}

/// Prints flappy's samples around its fourth restore and each good-wifi seed's last window.
#[test]
#[ignore = "diagnostics"]
fn diag_flappy_and_good_ends() {
    let mut r = Run::new(Leg::good_wifi(), Leg::good_wifi(), 3);
    for _ in 0..90_000 {
        let t = r.now;
        r.outage.set(t >= 15_000 && (t - 15_000) % 20_000 < 3_000);
        r.step();
        if (77_000..85_000).contains(&t) && t.is_multiple_of(250) {
            println!(
                "flappy t {t} c {:?} totals {:?}",
                r.client.quality(),
                r.client.status().link
            );
        }
    }
    for seed in 1..=8u64 {
        let mut r = Run::new(Leg::good_wifi(), Leg::good_wifi(), seed);
        r.run_ms(600_000);
        println!(
            "good seed {seed} up {:?} c {:?} s {:?}",
            r.up_counts.get(),
            r.client_quality(),
            r.server_quality()
        );
    }
}
