// SPDX-License-Identifier: MPL-2.0
//! Desync detection end to end (spec 120 § 7.6, ADR-8211): a listen host and three seats on
//! virtual-time links with a four-subsystem simulation that hashes through
//! [`crate::desync::DesyncHash`]. Seeded, no wall clock, no sleeps.

use std::collections::{BTreeMap, VecDeque};

use super::wire::{self, Order, SeatStream};
use super::*;
use crate::desync::{DesyncHash, DesyncInject, DesyncReport, ProbeRows, SubsystemRow};
use crate::payload::NetGame;

struct Game;
impl NetGame for Game {
    type Message = Vec<u8>;
    type Input = u8;
    type JoinData = ();
    const GAME_ID: &'static str = "test.desync";
    const PROTOCOL_VERSION: u32 = 1;
}

const NAMES: [&str; 4] = ["units", "economy", "orders", "rng"];

fn mix(mut h: u64, v: u64) -> u64 {
    h ^= v
        .wrapping_add(0x9e37_79b9_7f4a_7c15)
        .wrapping_add(h << 6)
        .wrapping_add(h >> 2);
    h.wrapping_mul(0x0000_0100_0000_01b3)
}

/// A deterministic simulation of four named subsystems.
#[derive(Debug, Clone, Default)]
struct Sim {
    state: [u64; 4],
    /// The state after each tick stepped (the newest step of a tick wins).
    history: BTreeMap<u64, [u64; 4]>,
    stepped: Vec<u64>,
    inject: Option<DesyncInject>,
    /// A live-only fault: the live step of this tick flips the `rng` state, a replay does not
    /// (the replay heals it).
    live_flip: Option<u64>,
}

impl DesyncHash for Sim {
    fn world_hash(&self, _tick: u64) -> u64 {
        self.state.iter().fold(0x00c0_ffee, |h, s| mix(h, *s))
    }
    fn subsystem_hashes(&self, _tick: u64, out: &mut Vec<(&'static str, u64)>) {
        out.clear();
        out.extend(NAMES.iter().copied().zip(self.state));
    }
}

impl Sim {
    fn fold(&mut self, frame: &LockstepFrame<u8>) {
        let tick = frame.tick;
        for (seat, input) in &frame.inputs {
            let v = input.as_ref().map_or(0xab, |b| u64::from(*b));
            self.state[0] = mix(self.state[0], u64::from(seat.0) ^ v);
            self.state[2] = mix(self.state[2], v.wrapping_mul(3) ^ tick);
        }
        self.state[1] = mix(self.state[1], tick.wrapping_mul(7) ^ self.state[0]);
        self.state[3] = mix(self.state[3], tick);
        if let Some(inject) = &self.inject {
            for (i, name) in NAMES.iter().enumerate() {
                self.state[i] = inject.flip(tick, name, self.state[i]);
            }
        }
        self.history.insert(tick, self.state);
    }

    fn step(&mut self, frame: &LockstepFrame<u8>) {
        self.stepped.push(frame.tick);
        self.fold(frame);
        if self.live_flip == Some(frame.tick) {
            self.state[3] ^= 0x5a5a;
            self.history.insert(frame.tick, self.state);
        }
    }

    /// The state after `tick` again, from the state after `from` (`None`: the start).
    fn rewind_to(&mut self, from: Option<u64>) {
        self.state = from
            .and_then(|t| self.history.get(&t).copied())
            .unwrap_or_default();
    }
}

/// How a seat (or the host's own seat) behaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Conduct {
    Honest,
    /// Votes a forged world hash from `from` on, whatever its state; never repairs.
    Forger {
        from: u64,
    },
}

fn forged(tick: u64, real: u64) -> u64 {
    mix(real, tick ^ 0xbad)
}

struct Link {
    latency_ms: u64,
    loss_per_mille: u64,
    rng: u64,
    queue: VecDeque<(u64, Lane, Vec<u8>)>,
}

impl Link {
    fn new(latency_ms: u64, loss_per_mille: u64, seed: u64) -> Self {
        Self {
            latency_ms,
            loss_per_mille,
            rng: seed,
            queue: VecDeque::new(),
        }
    }

    /// SplitMix64 (public domain, S. Vigna).
    fn next(&mut self) -> u64 {
        self.rng = self.rng.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.rng;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn send(&mut self, now: u64, out: Outgoing) {
        if out.lane == Lane::Datagram && self.next() % 1000 < self.loss_per_mille {
            return;
        }
        self.queue.push_back((now + self.latency_ms, out.lane, out.bytes));
    }

    fn due(&mut self, now: u64) -> Vec<(Lane, Vec<u8>)> {
        let mut out = Vec::new();
        while self.queue.front().is_some_and(|(at, _, _)| *at <= now) {
            if let Some((_, lane, bytes)) = self.queue.pop_front() {
                out.push((lane, bytes));
            }
        }
        out
    }
}

struct Node {
    seat: LockstepSeat<Game>,
    sim: Sim,
    conduct: Conduct,
    down: Link,
    up: Link,
    events: Vec<SeatEvent>,
}

struct Rig {
    now: u64,
    host: LockstepHost<Game>,
    host_sim: Sim,
    host_events: Vec<HostEvent>,
    nodes: BTreeMap<SeatId, Node>,
    /// Every simulation's subsystem hashes per tick, as stepped live: `tick -> (voter, row)`.
    rows: BTreeMap<u64, Vec<(Voter, [u64; 4])>>,
    host_refused: Vec<&'static str>,
    /// The bundles diverged seats wrote when told to take another state, before they did
    /// (spec 120 § 7.6, ADR-8455).
    seat_bundles: Vec<crate::desync::DesyncBundle>,
}

const LOCAL: SeatId = SeatId(0);

fn input(seat: SeatId, capture: u64) -> u8 {
    (capture / 9 % 5) as u8 ^ (seat.0 as u8)
}

impl Rig {
    fn new(config: LockstepConfig, loss_per_mille: u64) -> Self {
        let mut host = LockstepHost::<Game>::new(config.clone(), HostRole::Listen(LOCAL));
        host.add_seat(LOCAL);
        let mut nodes = BTreeMap::new();
        for id in 1..4u16 {
            let seat = SeatId(id);
            host.add_seat(seat);
            nodes.insert(
                seat,
                Node {
                    seat: LockstepSeat::new(config.clone(), seat),
                    sim: Sim::default(),
                    conduct: Conduct::Honest,
                    down: Link::new(15 + u64::from(id) * 5, loss_per_mille, 0x100 + u64::from(id)),
                    up: Link::new(15 + u64::from(id) * 5, loss_per_mille, 0x200 + u64::from(id)),
                    events: Vec::new(),
                },
            );
        }
        host.start(0);
        Self {
            now: 0,
            host,
            host_sim: Sim::default(),
            host_events: Vec::new(),
            nodes,
            rows: BTreeMap::new(),
            host_refused: Vec::new(),
            seat_bundles: Vec::new(),
        }
    }

    fn step_ms(&mut self) {
        let now = self.now;
        let mut transfers: Vec<(SeatId, u64, Voter)> = Vec::new();
        for (id, node) in &mut self.nodes {
            for (lane, bytes) in node.down.due(now) {
                node.seat.receive(lane, &bytes, now).expect("host payload");
            }
            if node.seat.removed() {
                continue;
            }
            if now == 0 {
                node.seat.submit_input(&input(*id, 0)).expect("first input");
            }
            while let Some(frame) = node.seat.next_frame() {
                node.sim.step(&frame);
                match node.conduct {
                    Conduct::Forger { from } if frame.tick >= from => {
                        let real = node.sim.world_hash(frame.tick);
                        node.seat.report_hash(frame.tick, forged(frame.tick, real));
                    }
                    _ => {
                        node.seat.record_state(frame.tick, &node.sim);
                    }
                }
                self.rows
                    .entry(frame.tick)
                    .or_default()
                    .push((Voter::Seat(*id), node.sim.state));
                node.seat
                    .submit_input(&input(*id, frame.tick + 1))
                    .expect("one input per capture");
            }
            while let Some(event) = node.seat.poll_event() {
                match event {
                    SeatEvent::SelfCheck { from, tick } => {
                        let frames: Vec<_> = node
                            .seat
                            .frames_since(from.map_or(0, |t| t + 1))
                            .cloned()
                            .collect();
                        node.sim.rewind_to(from);
                        let mut hash = None;
                        for frame in &frames {
                            node.sim.fold(frame);
                            let world = node.seat.record_replayed(frame.tick, &node.sim);
                            if frame.tick == tick {
                                hash = Some(world);
                            }
                        }
                        let hash = hash.unwrap_or(0);
                        let hash = match node.conduct {
                            Conduct::Forger { .. } => forged(tick, hash),
                            Conduct::Honest => hash,
                        };
                        node.seat.revote(tick, hash);
                    }
                    SeatEvent::Resync { tick, source } => {
                        // Before the transfer rewrites it: the seat's own bundle, from the last
                        // agreed checkpoint the host named in its self-check order.
                        let agreed = node.events.iter().rev().find_map(|e| match e {
                            SeatEvent::SelfCheck { from, .. } => Some(*from),
                            _ => None,
                        });
                        if let Some(from) = agreed {
                            let snapshot = bundle_snapshot(&node.sim, from);
                            if let Ok(bundle) = crate::desync::DesyncBundle::from_seat(&node.seat, snapshot) {
                                self.seat_bundles.push(bundle);
                            }
                        }
                        transfers.push((*id, tick, source));
                    }
                    _ => {}
                }
                node.events.push(event);
            }
            if now % 16 == u64::from(id.0) * 3 {
                for out in node.seat.outgoing(now) {
                    node.up.send(now, out);
                }
            }
        }
        // The state transfer (the late-join path's job): copy the source's state at the checkpoint,
        // replay the frames after it.
        for (target, tick, source) in transfers {
            let state = match source {
                Voter::Seat(s) if s == LOCAL => self.host_sim.history.get(&tick).copied(),
                Voter::Seat(s) => self.nodes.get(&s).and_then(|n| n.sim.history.get(&tick).copied()),
                Voter::Observer => None,
            };
            let (Some(state), Some(node)) = (state, self.nodes.get_mut(&target)) else {
                continue;
            };
            node.sim.state = state;
            node.sim.history.insert(tick, state);
            let at_checkpoint = node.seat.record_replayed(tick, &node.sim);
            let frames: Vec<_> = node.seat.frames_since(tick + 1).cloned().collect();
            for frame in &frames {
                node.sim.fold(frame);
                node.seat.record_replayed(frame.tick, &node.sim);
            }
            let hash = match node.conduct {
                Conduct::Forger { .. } => forged(tick, at_checkpoint),
                Conduct::Honest => at_checkpoint,
            };
            node.seat.resynced(tick, hash);
        }
        for (id, node) in &mut self.nodes {
            for (lane, bytes) in node.up.due(now) {
                match self.host.receive(*id, lane, &bytes, now) {
                    Ok(()) | Err(LockstepError::UnknownSeat(_)) => {}
                    Err(e) => self.host_refused.push(e.code()),
                }
            }
        }
        if now == 0 {
            self.host.local_input(&input(LOCAL, 0), now).expect("host input");
        }
        while let Some(frame) = self.host.poll_frame(now) {
            self.host_sim.step(&frame);
            self.host.record_state(frame.tick, &self.host_sim);
            self.rows
                .entry(frame.tick)
                .or_default()
                .push((Voter::Seat(LOCAL), self.host_sim.state));
            self.host
                .local_input(&input(LOCAL, frame.tick + 1), now)
                .expect("host input");
        }
        while let Some(event) = self.host.poll_event() {
            if let HostEvent::SelfCheck { from, tick } = &event {
                let frames: Vec<_> = self
                    .host
                    .frames_since(from.map_or(0, |t| t + 1))
                    .cloned()
                    .collect();
                self.host_sim.rewind_to(*from);
                let mut hash = 0;
                for frame in &frames {
                    self.host_sim.fold(frame);
                    let world = self.host.record_replayed(frame.tick, &self.host_sim);
                    if frame.tick == *tick {
                        hash = world;
                    }
                }
                self.host.revote(*tick, hash);
            }
            self.host_events.push(event);
        }
        if now.is_multiple_of(16) {
            for (seat, out) in self.host.outgoing(now) {
                if let Some(node) = self.nodes.get_mut(&seat) {
                    node.down.send(now, out);
                }
            }
        }
        self.now += 1;
    }

    /// Runs until the host has built `ticks` frames (60 Hz: about 16.7 ms each).
    fn run_ticks(&mut self, ticks: u64) {
        while self.host.tick() < ticks {
            self.step_ms();
            assert!(
                self.now < ticks * 40 + 10_000,
                "the match stalled at tick {}",
                self.host.tick()
            );
        }
        // Let the last frames and votes land.
        for _ in 0..300 {
            self.step_ms();
        }
    }

    fn verdicts(&self) -> Vec<&DesyncVerdict> {
        self.host_events
            .iter()
            .filter_map(|e| match e {
                HostEvent::Desync(v) => Some(v),
                _ => None,
            })
            .collect()
    }

    fn reports(&self) -> Vec<&DesyncReport> {
        self.host_events
            .iter()
            .filter_map(|e| match e {
                HostEvent::DesyncProbed(r) => Some(r),
                _ => None,
            })
            .collect()
    }

    fn count(&self, pred: impl Fn(&HostEvent) -> bool) -> usize {
        self.host_events.iter().filter(|e| pred(e)).count()
    }

    /// Every simulation still in the match stepped `0..n` in order, each once.
    fn assert_stepped_in_order(&self, seats: &[u16], at_least: u64) {
        for id in seats {
            let sim = if *id == LOCAL.0 {
                &self.host_sim
            } else {
                &self.nodes[&SeatId(*id)].sim
            };
            assert!(
                sim.stepped.iter().copied().eq(0..sim.stepped.len() as u64),
                "seat{id} stepped out of order or twice"
            );
            assert!(
                sim.stepped.len() as u64 >= at_least,
                "seat{id} stopped at {} of {at_least}",
                sim.stepped.len()
            );
        }
    }
}

fn config() -> LockstepConfig {
    LockstepConfig::default()
}

/// Acceptance (spec 120 § 7.6): four seats agree on every subsystem hash for 600 ticks; every
/// checkpoint is agreed and nobody is probed.
#[test]
fn four_seats_agree_on_every_subsystem_hash_for_600_ticks() {
    let mut rig = Rig::new(config(), 0);
    rig.run_ticks(600);
    for tick in 0..600u64 {
        let rows = &rig.rows[&tick];
        assert_eq!(rows.len(), 4, "tick {tick}: every simulation stepped it once");
        assert!(rows.iter().all(|(_, r)| *r == rows[0].1), "tick {tick}: {rows:?}");
    }
    // The rings hold the newest 240 ticks of the same rows everywhere.
    let host_rows = rig.host.subsystem_rows();
    assert_eq!(host_rows.names(), NAMES);
    assert_eq!(host_rows.len(), crate::desync::ROW_RING_TICKS);
    for node in rig.nodes.values() {
        let ring = node.seat.subsystem_rows();
        let (first, last) = ring.span().expect("rows");
        assert_eq!(ring.len(), crate::desync::ROW_RING_TICKS);
        let mut compared = 0;
        for t in first..=last {
            // The host builds ahead of the seats, so its ring starts a few ticks later.
            if let (Some(mine), Some(host)) = (ring.row(t), host_rows.row(t)) {
                assert_eq!(mine, host, "tick {t}");
                compared += 1;
            }
        }
        assert!(compared >= 200, "{compared} rows compared");
    }
    assert!(rig.verdicts().is_empty(), "{:?}", rig.verdicts());
    assert!(rig.reports().is_empty());
    let status = rig.host.status(rig.now);
    assert!(status.checkpoints_agreed >= 19, "{status:?}");
    assert_eq!((status.desyncs, status.resyncs, status.desync_reports), (0, 0, 0));
    assert!(rig.host_refused.is_empty(), "{:?}", rig.host_refused);
}

/// Acceptance: a desync injected on one seat at tick 100 is detected by the next hash tick (120),
/// the probe names the subsystem, the first diverged tick and the last agreed tick, the seat is
/// resynced from the majority and the match goes on with every checkpoint agreed after it.
#[test]
fn an_injected_desync_is_named_by_the_next_hash_tick_and_the_seat_is_resynced() {
    let mut rig = Rig::new(config(), 0);
    let victim = SeatId(2);
    if let Some(node) = rig.nodes.get_mut(&victim) {
        node.sim.inject = Some(DesyncInject::new("economy", 100));
    }
    rig.run_ticks(600);
    let verdicts = rig.verdicts();
    assert_eq!(verdicts.len(), 1, "{verdicts:?}");
    let v = verdicts[0];
    assert_eq!(v.kind, VerdictKind::Diverged);
    assert_eq!(v.tick, 120, "detected by the next hash tick");
    assert_eq!(v.last_agreed, Some(90));
    assert_eq!(v.diverged, [Voter::Seat(victim)]);
    assert!(!v.host_diverged);
    let reports = rig.reports();
    assert_eq!(reports.len(), 1, "{reports:?}");
    let r = reports[0];
    assert_eq!(
        (
            r.checkpoint,
            r.voter,
            r.reference,
            r.first_diverged_tick,
            r.last_agreed_tick
        ),
        (120, Voter::Seat(victim), Voter::Seat(LOCAL), Some(100), Some(99)),
        "{r:?}"
    );
    assert_eq!(r.subsystem.as_deref(), Some("economy"));
    assert_eq!(r.subsystems, ["economy"]);
    assert_eq!(
        rig.count(|e| matches!(e, HostEvent::ResyncSeat { seat, counted: true, .. } if *seat == victim)),
        1,
        "resynced once"
    );
    assert_eq!(rig.count(|e| matches!(e, HostEvent::SeatRemoved { .. })), 0);
    assert_eq!(
        rig.count(|e| matches!(e, HostEvent::Unresolved { .. } | HostEvent::HostFault { .. })),
        0
    );
    rig.assert_stepped_in_order(&[0, 1, 2, 3], 600);
    // Back in step: the newest ticks agree everywhere and the seat is no longer under a transfer.
    for tick in 500..600u64 {
        let rows = &rig.rows[&tick];
        assert!(rows.iter().all(|(_, r)| *r == rows[0].1), "tick {tick}");
    }
    let status = rig.host.status(rig.now);
    let row = status
        .seats
        .iter()
        .find(|s| s.seat == victim.0)
        .expect("seat row");
    assert_eq!((row.resyncs, row.resyncing), (1, false));
    assert_eq!(status.desync_reports, 1);
    assert_eq!(
        status.last_desync.as_ref().and_then(|r| r.subsystem.as_deref()),
        Some("economy")
    );
    assert!(status.last_agreed_tick >= Some(570), "{status:?}");
}

/// A desync at a checkpoint tick: the state transfer ordered by the same verdict replaces the
/// seat's row at that tick before the probe's second stage asks for it. The rows kept when the
/// first stage arrived still name the subsystem.
#[test]
fn a_desync_at_a_checkpoint_tick_is_named_though_the_transfer_rewrote_the_ring() {
    let mut rig = Rig::new(config(), 0);
    let victim = SeatId(1);
    if let Some(node) = rig.nodes.get_mut(&victim) {
        node.sim.inject = Some(DesyncInject::new("rng", 150));
    }
    // Short enough that both rings still hold tick 150.
    rig.run_ticks(300);
    let verdicts = rig.verdicts();
    assert_eq!(verdicts.len(), 1, "{verdicts:?}");
    assert_eq!((verdicts[0].tick, verdicts[0].last_agreed), (150, Some(120)));
    // The transfer took the reference's state at 150: the ring now holds the agreeing row there.
    let ring = rig.nodes[&victim].seat.subsystem_rows();
    assert_eq!(
        ring.row(150),
        rig.host.subsystem_rows().row(150),
        "the ring was rewritten"
    );
    let reports = rig.reports();
    assert_eq!(reports.len(), 1, "{reports:?}");
    assert_eq!(
        (
            reports[0].voter,
            reports[0].first_diverged_tick,
            reports[0].last_agreed_tick,
            reports[0].subsystem.as_deref()
        ),
        (Voter::Seat(victim), Some(150), Some(149), Some("rng"))
    );
    rig.assert_stepped_in_order(&[0, 1, 2, 3], 300);
}

/// Two of four seats hit at once are no strict majority: a split decided for the host's group,
/// counted against nobody, removing nobody, and no probe (nobody is known to be wrong).
#[test]
fn two_of_four_hit_at_once_is_a_split_that_probes_and_removes_nobody() {
    let mut rig = Rig::new(config(), 0);
    if let Some(node) = rig.nodes.get_mut(&SeatId(1)) {
        node.sim.inject = Some(DesyncInject::new("orders", 200));
    }
    if let Some(node) = rig.nodes.get_mut(&SeatId(3)) {
        node.sim.inject = Some(DesyncInject::new("orders", 200));
    }
    rig.run_ticks(600);
    let verdicts = rig.verdicts();
    assert_eq!(verdicts.len(), 1, "{verdicts:?}");
    assert_eq!(verdicts[0].kind, VerdictKind::Split);
    assert!(rig.reports().is_empty());
    assert_eq!(rig.count(|e| matches!(e, HostEvent::SeatRemoved { .. })), 0);
    assert_eq!(
        rig.count(|e| matches!(e, HostEvent::ResyncSeat { counted: true, .. })),
        0,
        "a tie is held against nobody"
    );
    rig.assert_stepped_in_order(&[0, 1, 2, 3], 600);
}

/// The probe pulls rows from every diverged voter: in a five-voter match (an observer host and four
/// seats) two seats hit in different subsystems at different ticks are a minority, and each is
/// named with its own subsystem, tick and last agreed tick.
#[test]
fn an_observer_host_probes_both_diverged_seats() {
    let config = config();
    let mut host = LockstepHost::<Game>::new(config.clone(), HostRole::Observer);
    let mut seats: Vec<LockstepSeat<Game>> = (1..=4u16)
        .map(|i| LockstepSeat::new(config.clone(), SeatId(i)))
        .collect();
    let mut sims: Vec<Sim> = vec![Sim::default(); 4];
    sims[0].inject = Some(DesyncInject::new("orders", 200));
    sims[3].inject = Some(DesyncInject::new("units", 205));
    let mut host_sim = Sim::default();
    for s in &seats {
        host.add_seat(s.seat());
    }
    host.start(0);
    let mut events = Vec::new();
    for now in 0..9_000u64 {
        for (i, seat) in seats.iter_mut().enumerate() {
            if now == 0 {
                seat.submit_input(&1).expect("input");
            }
            while let Some(frame) = seat.next_frame() {
                sims[i].step(&frame);
                seat.record_state(frame.tick, &sims[i]);
                seat.submit_input(&(i as u8)).expect("input");
            }
            while let Some(e) = seat.poll_event() {
                if let SeatEvent::SelfCheck { from, tick } = e {
                    let frames: Vec<_> = seat.frames_since(from.map_or(0, |t| t + 1)).cloned().collect();
                    sims[i].rewind_to(from);
                    let mut hash = 0;
                    for f in &frames {
                        sims[i].fold(f);
                        let w = seat.record_replayed(f.tick, &sims[i]);
                        if f.tick == tick {
                            hash = w;
                        }
                    }
                    seat.revote(tick, hash);
                }
            }
            if now.is_multiple_of(16) {
                for out in seat.outgoing(now) {
                    host.receive(seat.seat(), out.lane, &out.bytes, now)
                        .expect("seat bytes");
                }
            }
        }
        while let Some(frame) = host.poll_frame(now) {
            host_sim.step(&frame);
            host.record_state(frame.tick, &host_sim);
        }
        while let Some(e) = host.poll_event() {
            events.push(e);
        }
        if now.is_multiple_of(16) {
            for (to, out) in host.outgoing(now) {
                let i = usize::from(to.0) - 1;
                seats[i].receive(out.lane, &out.bytes, now).expect("host bytes");
            }
        }
        if host.tick() > 400 {
            break;
        }
    }
    let mut reports: Vec<DesyncReport> = events
        .iter()
        .filter_map(|e| match e {
            HostEvent::DesyncProbed(r) => Some(r.clone()),
            _ => None,
        })
        .collect();
    reports.sort_by_key(|r| r.voter);
    let expect = |seat: u16, tick: u64, subsystem: &str| DesyncReport {
        checkpoint: 210,
        voter: Voter::Seat(SeatId(seat)),
        reference: Voter::Observer,
        first_diverged_tick: Some(tick),
        last_agreed_tick: Some(tick - 1),
        subsystem: Some(subsystem.to_owned()),
        subsystems: vec![subsystem.to_owned()],
    };
    assert_eq!(
        reports,
        [expect(1, 200, "orders"), expect(4, 205, "units")],
        "{events:?}"
    );
}

/// Acceptance (hostile): one seat votes forged hashes from tick 60 on, revotes forged and claims
/// forged transfers. It is resynced up to `max_resyncs`, then removed with `desync`; the other
/// seats never stop stepping, nobody else is resynced, and the match is never unresolved.
#[test]
fn a_seat_that_forges_hashes_is_resynced_then_removed_and_the_others_never_stop() {
    let mut rig = Rig::new(config(), 0);
    let forger = SeatId(3);
    if let Some(node) = rig.nodes.get_mut(&forger) {
        node.conduct = Conduct::Forger { from: 60 };
    }
    rig.run_ticks(2_400);
    let resyncs: Vec<(u32, bool)> = rig
        .host_events
        .iter()
        .filter_map(|e| match e {
            HostEvent::ResyncSeat {
                seat,
                resyncs,
                counted,
                ..
            } => {
                assert_eq!(*seat, forger, "only the forger is resynced");
                Some((*resyncs, *counted))
            }
            _ => None,
        })
        .collect();
    assert_eq!(resyncs, [(1, true), (2, true)], "resynced up to the limit");
    let removed: Vec<(SeatId, &str)> = rig
        .host_events
        .iter()
        .filter_map(|e| match e {
            HostEvent::SeatRemoved { seat, reason, .. } => Some((*seat, *reason)),
            _ => None,
        })
        .collect();
    assert_eq!(removed, [(forger, crate::desync::DESYNC)]);
    assert!(rig.nodes[&forger].seat.removed(), "the forger was told");
    assert_eq!(
        rig.count(|e| matches!(
            e,
            HostEvent::Unresolved { .. } | HostEvent::HostFault { .. } | HostEvent::ResyncHost { .. }
        )),
        0
    );
    // The others never stopped: every tick in order, through the end.
    rig.assert_stepped_in_order(&[0, 1, 2], 2_400);
    for tick in 0..2_400u64 {
        let honest: Vec<[u64; 4]> = rig.rows[&tick]
            .iter()
            .filter(|(v, _)| *v != Voter::Seat(forger))
            .map(|(_, r)| *r)
            .collect();
        assert_eq!(honest.len(), 3, "tick {tick}");
        assert!(honest.iter().all(|r| *r == honest[0]), "tick {tick}");
    }
    let status = rig.host.status(rig.now);
    assert!(!status.seats.iter().any(|s| s.seat == forger.0), "{status:?}");
    assert!(status.seats.iter().all(|s| s.resyncs == 0));
    assert!(status.last_agreed_tick >= Some(2_340), "{status:?}");
    // The forger's own rows hold no forged world hash, so the probe found nothing to name in them;
    // a report, if any, names the forger only.
    assert!(rig.reports().iter().all(|r| r.voter == Voter::Seat(forger)));
}

/// The same hostile seat with the delay and loss of a bad link: the verdict still needs the
/// majority, so the forger is the only one removed.
#[test]
fn a_forger_on_a_lossy_link_is_still_the_only_one_removed() {
    let mut rig = Rig::new(config(), 30);
    let forger = SeatId(1);
    if let Some(node) = rig.nodes.get_mut(&forger) {
        node.conduct = Conduct::Forger { from: 300 };
    }
    rig.run_ticks(2_700);
    let removed: Vec<SeatId> = rig
        .host_events
        .iter()
        .filter_map(|e| match e {
            HostEvent::SeatRemoved { seat, .. } => Some(*seat),
            _ => None,
        })
        .collect();
    assert_eq!(removed, [forger]);
    assert!(
        !rig.host_events
            .iter()
            .any(|e| matches!(e, HostEvent::ResyncSeat { seat, .. } if *seat != forger)),
        "nobody else resynced"
    );
    rig.assert_stepped_in_order(&[0, 2, 3], 2_700);
}

/// Hostile probe replies: rows nobody asked for are dropped, malformed rows are refused, and
/// neither stops the match or makes a report.
#[test]
fn unasked_and_malformed_subsystem_rows_are_dropped_and_the_match_goes_on() {
    let mut rig = Rig::new(config(), 0);
    rig.run_ticks(120);
    let unasked = wire::encode_seat_stream(&SeatStream::SubsystemRows(ProbeRows {
        names: vec!["units".to_owned()],
        rows: vec![SubsystemRow {
            tick: 10,
            world: 1,
            hashes: vec![2],
        }],
    }));
    rig.host
        .receive(SeatId(1), Lane::Stream, &unasked, rig.now)
        .expect("well-formed rows are taken (and dropped: nothing asked for them)");
    let mut refused = 0;
    for bad in hostile_subsystem_rows() {
        if let Err(e) = rig.host.receive(SeatId(2), Lane::Stream, &bad, rig.now) {
            assert_eq!(e.code(), "lockstep_malformed");
            refused += 1;
        }
    }
    assert_eq!(refused, hostile_subsystem_rows().len());
    rig.run_ticks(300);
    assert!(rig.reports().is_empty());
    assert!(rig.verdicts().is_empty());
    rig.assert_stepped_in_order(&[0, 1, 2, 3], 300);
}

fn hostile_subsystem_rows() -> Vec<Vec<u8>> {
    let kind = wire::KIND_SUBSYSTEM_ROWS;
    let mut too_many_names = vec![kind, 65];
    too_many_names.extend(std::iter::repeat_n([1u8, b'a'], 65).flatten());
    let mut long_name = vec![kind, 1, 65];
    long_name.extend([b'x'; 65]);
    vec![
        // Truncated after the kind.
        vec![kind],
        too_many_names,
        long_name,
        // A name that is not UTF-8.
        vec![kind, 1, 2, 0xff, 0xfe, 0],
        // The same name twice.
        vec![kind, 2, 1, b'a', 1, b'a', 0],
        // 17 rows (more than a reply may carry).
        vec![kind, 0, 17],
        // Rows out of order: tick 5 then tick 4.
        [vec![kind, 0, 2, 5], vec![0; 8], vec![4], vec![0; 8]].concat(),
        // A row shorter than its names say.
        [vec![kind, 1, 1, b'a', 1, 3], vec![0; 8], vec![0; 4]].concat(),
        // Trailing bytes.
        vec![kind, 0, 0, 9],
    ]
}

/// The subsystem rows and the new order round-trip; every byte string decodes or is refused
/// without a panic.
#[test]
fn subsystem_rows_and_the_subsystem_probe_order_round_trip_and_never_panic() {
    let rows = ProbeRows {
        names: NAMES.iter().map(|n| (*n).to_owned()).collect(),
        rows: (40..42)
            .map(|t| SubsystemRow {
                tick: t,
                world: t * 11,
                hashes: vec![t, t + 1, t + 2, t + 3],
            })
            .collect(),
    };
    let bytes = wire::encode_seat_stream(&SeatStream::SubsystemRows(rows.clone()));
    assert_eq!(
        wire::decode_seat_stream(&bytes),
        Ok(SeatStream::SubsystemRows(rows))
    );
    let order = Order::ProbeSubsystems { from: 99, to: 100 };
    assert_eq!(wire::decode_order(&wire::encode_order(&order)), Ok(order));
    // A range wider than a reply may carry is refused.
    let wide = wire::encode_order(&Order::ProbeSubsystems { from: 0, to: 16 });
    assert!(wire::decode_order(&wide).is_err());
    // Over-long input is cut on encode so what is sent decodes.
    let long = ProbeRows {
        names: (0..80).map(|i| format!("{i:02}{}", "n".repeat(70))).collect(),
        rows: (0..40)
            .map(|t| SubsystemRow {
                tick: t,
                world: 0,
                hashes: vec![1; 80],
            })
            .collect(),
    };
    let bytes = wire::encode_seat_stream(&SeatStream::SubsystemRows(long));
    match wire::decode_seat_stream(&bytes) {
        Ok(SeatStream::SubsystemRows(back)) => {
            assert_eq!(back.names.len(), crate::desync::MAX_SUBSYSTEMS);
            assert_eq!(back.rows.len(), wire::MAX_SUBSYSTEM_ROWS);
        }
        other => panic!("{other:?}"),
    }
    // Every prefix and a seeded spray of byte flips: refused or decoded, never a panic.
    let mut link = Link::new(0, 0, 77);
    for cut in 0..bytes.len() {
        let _ = wire::decode_seat_stream(&bytes[..cut]);
    }
    for _ in 0..2_000 {
        let mut b = bytes.clone();
        let at = (link.next() as usize) % b.len();
        b[at] ^= (link.next() as u8) | 1;
        let _ = wire::decode_seat_stream(&b);
    }
}

// ---- The desync bundle and its offline replay (spec 120 § 7.6, § 12.4; ADR-8455) ----

use crate::desync::{BundleFrame, BundleSnapshot, DesyncBundle, DesyncReplay, GameIdentity, replay};

/// The snapshot of `sim` after `from` (`None`: the match start), from its stepped history.
fn bundle_snapshot(sim: &Sim, from: Option<u64>) -> BundleSnapshot {
    match from.and_then(|t| sim.history.get(&t).map(|state| (t, state))) {
        Some((tick, state)) => BundleSnapshot::at(tick, state.iter().flat_map(|w| w.to_le_bytes()).collect()),
        None => BundleSnapshot::start(),
    }
}

/// The test simulation as a replay hook: the same `fold` as the live step, fed from the bundle's
/// input bytes. No injection: the replay is what the inputs alone produce.
#[derive(Default)]
struct ReplaySim(Sim);

impl DesyncHash for ReplaySim {
    fn world_hash(&self, tick: u64) -> u64 {
        self.0.world_hash(tick)
    }
    fn subsystem_hashes(&self, tick: u64, out: &mut Vec<(&'static str, u64)>) {
        self.0.subsystem_hashes(tick, out);
    }
}

impl DesyncReplay for ReplaySim {
    fn identity(&self) -> GameIdentity {
        GameIdentity::of::<Game>()
    }
    fn load(&mut self, snapshot: &BundleSnapshot) -> Result<(), String> {
        self.0 = Sim::default();
        if snapshot.tick.is_none() {
            return Ok(());
        }
        let (words, rest) = snapshot.data.0.as_chunks::<8>();
        if words.len() != 4 || !rest.is_empty() {
            return Err(format!("{} snapshot bytes", snapshot.data.0.len()));
        }
        for (slot, word) in self.0.state.iter_mut().zip(words) {
            *slot = u64::from_le_bytes(*word);
        }
        Ok(())
    }
    fn step(&mut self, frame: &BundleFrame) -> Result<(), String> {
        let inputs = frame.decode::<u8>().map_err(|e| e.to_string())?;
        self.0.fold(&LockstepFrame {
            tick: frame.tick,
            inputs: inputs
                .into_iter()
                .map(|(seat, input)| (SeatId(seat), input.map_or(SeatInput::Absent, SeatInput::Present)))
                .collect(),
            delay: 0,
            delay_from: 0,
            membership: Vec::new(),
            match_end: false,
        });
        Ok(())
    }
}

/// Runs a match with `inject` planted on `victim`, returns the rig and the host's bundle written
/// when the probe named the subsystem (snapshot: the host's state at the verdict's last agreed
/// checkpoint).
fn injected_match(
    loss_per_mille: u64,
    victim: SeatId,
    inject: DesyncInject,
    ticks: u64,
) -> (Rig, DesyncBundle) {
    let mut rig = Rig::new(config(), loss_per_mille);
    if let Some(node) = rig.nodes.get_mut(&victim) {
        node.sim.inject = Some(inject);
    }
    let mut bundle = None;
    while rig.host.tick() < ticks {
        let seen = rig.host_events.len();
        rig.step_ms();
        assert!(
            rig.now < ticks * 40 + 10_000,
            "the match stalled at tick {}",
            rig.host.tick()
        );
        // The host application writes the bundle when the probe reports.
        if bundle.is_none()
            && rig.host_events[seen..]
                .iter()
                .any(|e| matches!(e, HostEvent::DesyncProbed(_)))
        {
            let agreed = rig.verdicts().first().and_then(|v| v.last_agreed);
            let snapshot = bundle_snapshot(&rig.host_sim, agreed);
            bundle = Some(DesyncBundle::from_host(&rig.host, snapshot).expect("host bundle"));
        }
    }
    let bundle = bundle.expect("the probe reported and the host wrote a bundle");
    (rig, bundle)
}

/// Acceptance (N227, spec 120 § 7.6): a desync injected on one seat is detected and probed live;
/// the host's bundle, written to RON and read back, replays offline through the game's step to
/// the same seat, tick and subsystem the live probe named. Clean link and 3 % datagram loss.
#[test]
fn an_injected_desyncs_bundle_replays_offline_to_the_same_tick_and_subsystem() {
    for loss in [0, 30] {
        let victim = SeatId(2);
        let (rig, bundle) = injected_match(loss, victim, DesyncInject::new("economy", 100), 400);
        let live = rig.reports()[0].clone();
        assert_eq!(
            (live.voter, live.first_diverged_tick, live.subsystem.as_deref()),
            (Voter::Seat(victim), Some(100), Some("economy")),
            "loss {loss}: {live:?}"
        );
        assert_eq!(bundle.writer, crate::desync::BundleWriter::Host);
        assert_eq!(bundle.snapshot.tick, Some(90), "the last agreed checkpoint");
        assert_eq!(bundle.frames.first().map(|f| f.tick), Some(91));
        assert_eq!(bundle.verdict.as_ref(), Some(&live));
        let voters: Vec<Voter> = bundle.peers.iter().map(|p| p.voter).collect();
        assert_eq!(
            voters,
            [Voter::Seat(LOCAL), Voter::Seat(victim)],
            "sorted: the reference and the victim"
        );

        let text = bundle.to_ron().expect("ron");
        let read = DesyncBundle::from_ron(&text).expect("read back");
        let verdict = replay(&read, &mut ReplaySim::default()).expect("replay");
        assert_eq!(
            verdict.voter,
            Some(Voter::Seat(victim)),
            "loss {loss}: {verdict:?}"
        );
        assert_eq!(verdict.first_diverging_tick, Some(100));
        assert_eq!(verdict.subsystem.as_deref(), Some("economy"));
        assert_eq!(verdict.subsystems, ["economy"]);
        assert_eq!(verdict.last_agreed_tick, Some(99));
        assert_eq!(verdict.matches_live, Some(true));
        // The host's own simulation agrees with the replay on every tick it hashed since the
        // snapshot: the replay reproduces the live step exactly.
        let host = &verdict.peers[0];
        assert_eq!(host.first_diverged_tick, None, "{host:?}");
        // Written when the probe reported (about tick 125): every tick since the snapshot.
        assert!(host.rows_compared >= 30, "{host:?}");
    }
}

/// The diverged seat writes its own bundle when it is told to take the majority's state, before it
/// does; replayed offline, its rows name the tick and subsystem it planted.
#[test]
fn the_diverged_seat_writes_its_own_bundle_and_it_replays_to_the_planted_tick() {
    let victim = SeatId(3);
    let (rig, _) = injected_match(0, victim, DesyncInject::new("orders", 140), 300);
    assert_eq!(rig.seat_bundles.len(), 1, "one transfer, one seat bundle");
    let bundle = &rig.seat_bundles[0];
    assert_eq!(bundle.writer, crate::desync::BundleWriter::Seat(victim.0));
    assert_eq!(
        bundle.snapshot.tick,
        Some(120),
        "the last agreed checkpoint before 140"
    );
    assert_eq!(bundle.verdict, None, "a seat has no probe report");
    let read = DesyncBundle::from_ron(&bundle.to_ron().expect("ron")).expect("read back");
    let verdict = replay(&read, &mut ReplaySim::default()).expect("replay");
    assert_eq!(
        (
            verdict.voter,
            verdict.first_diverging_tick,
            verdict.subsystem.as_deref()
        ),
        (Some(Voter::Seat(victim)), Some(140), Some("orders")),
        "{verdict:?}"
    );
    assert_eq!(verdict.matches_live, None);
}

/// Without a desync there is nothing to name: a bundle of a clean match replays with every peer
/// agreeing (the replay is not a source of false verdicts).
#[test]
fn a_clean_matchs_bundle_replays_without_a_verdict() {
    let mut rig = Rig::new(config(), 0);
    rig.run_ticks(200);
    let bundle =
        DesyncBundle::from_host(&rig.host, bundle_snapshot(&rig.host_sim, Some(60))).expect("bundle");
    assert!(bundle.verdict.is_none());
    let verdict = replay(&bundle, &mut ReplaySim::default()).expect("replay");
    assert_eq!(verdict.first_diverging_tick, None, "{verdict:?}");
    assert!(verdict.peers.iter().all(|p| p.rows_compared > 100), "{verdict:?}");
}

/// The probe's hash ring (ADR-10147): one hash per tick, ascending; a tick reported again takes
/// the newest hash in place; a tick older than the newest goes in tick order; a full ring drops
/// its oldest tick and never stores one older than all it keeps.
#[test]
fn the_probe_ring_keeps_one_hash_per_tick_and_takes_the_newest() {
    use super::host::{HASH_RING_TICKS, put_ring_hash};
    let mut ring = VecDeque::new();
    for t in 0..10u64 {
        put_ring_hash(&mut ring, t, t);
    }
    // A replay of 4..=9 after a state transfer.
    for t in 4..10u64 {
        put_ring_hash(&mut ring, t, 100 + t);
    }
    let want: Vec<(u64, u64)> = (0..10u64).map(|t| (t, if t < 4 { t } else { 100 + t })).collect();
    assert!(ring.iter().copied().eq(want.iter().copied()), "{ring:?}");
    // A hole is filled in tick order.
    ring.retain(|(t, _)| *t != 6);
    put_ring_hash(&mut ring, 6, 66);
    assert!(ring.iter().map(|(t, _)| *t).eq(0..10), "{ring:?}");
    assert_eq!(ring[6], (6, 66));
    // Full: live ticks drop the oldest; a replayed tick replaces in place.
    let mut ring = VecDeque::new();
    let n = HASH_RING_TICKS as u64;
    for t in 0..n + 5 {
        put_ring_hash(&mut ring, t, t);
    }
    assert_eq!(ring.len(), HASH_RING_TICKS);
    assert_eq!(ring.front(), Some(&(5, 5)));
    put_ring_hash(&mut ring, n, 7);
    assert_eq!(ring.len(), HASH_RING_TICKS);
    assert_eq!(ring.iter().find(|(t, _)| *t == n), Some(&(n, 7)));
    // Full, a tick older than every kept one: not stored.
    put_ring_hash(&mut ring, 2, 2);
    assert_eq!(ring.front(), Some(&(5, 5)));
    // Full, a missing tick inside the span: the oldest goes, the tick goes in order.
    ring.retain(|(t, _)| *t != 50);
    put_ring_hash(&mut ring, n + 5, n + 5);
    put_ring_hash(&mut ring, 50, 500);
    assert_eq!(ring.len(), HASH_RING_TICKS);
    assert!(
        ring.iter().zip(ring.iter().skip(1)).all(|(a, b)| a.0 < b.0),
        "ascending"
    );
    assert_eq!(ring.iter().find(|(t, _)| *t == 50), Some(&(50, 500)));
}

/// ADR-10147: the host's simulation goes wrong on its live step of tick 100 only, and its
/// self-check replay from checkpoint 90 heals it (`record_replayed`). The replayed hashes replace
/// the live ones in the host's probe ring, so when seat 2 drifts at tick 125 the next probe, with
/// the host as the reference, names 125: not 121, the first tick the ring still held from before
/// the replay.
#[test]
fn a_healed_host_self_check_does_not_move_the_next_seats_first_diverged_tick() {
    let mut rig = Rig::new(config(), 0);
    rig.host_sim.live_flip = Some(100);
    let victim = SeatId(2);
    if let Some(node) = rig.nodes.get_mut(&victim) {
        node.sim.inject = Some(DesyncInject::new("economy", 125));
    }
    rig.run_ticks(300);
    assert!(
        rig.count(|e| matches!(e, HostEvent::SelfCheck { tick: 120, .. })) >= 1,
        "the host rechecked checkpoint 120: {:?}",
        rig.verdicts()
    );
    let later = rig
        .verdicts()
        .into_iter()
        .find(|v| v.tick > 125 && v.kind == VerdictKind::Diverged)
        .cloned()
        .unwrap_or_else(|| panic!("seat 2 is outvoted: {:?}", rig.verdicts()));
    assert_eq!(later.diverged, [Voter::Seat(victim)], "{later:?}");
    assert_eq!(later.source, Some(Voter::Seat(LOCAL)), "{later:?}");
    assert!(later.last_agreed.is_some_and(|t| t >= 120), "{later:?}");
    let located: Vec<(Voter, u64, u64)> = rig
        .host_events
        .iter()
        .filter_map(|e| match e {
            HostEvent::DesyncLocated {
                tick,
                voter,
                first_diverged_tick,
            } => Some((*voter, *tick, *first_diverged_tick)),
            _ => None,
        })
        .collect();
    assert_eq!(
        located,
        [(Voter::Seat(victim), later.tick, 125)],
        "the probe names seat 2's drift, not a tick the host's ring held from before its replay: {later:?}"
    );
    rig.assert_stepped_in_order(&[0, 1, 2, 3], 300);
}
