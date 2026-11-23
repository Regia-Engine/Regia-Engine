// SPDX-License-Identifier: MPL-2.0
//! Host migration end to end (spec 120 § 10.5; ADR-8809): processes with a seat each, one of them
//! hosting, on seeded virtual-time links (no wall clock, no sleeps). DO27's migration scenarios.

use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, Ipv4Addr};

use super::migration::{TEST_KIND_CLAIM, TEST_KIND_ELECT, TEST_KIND_HELLO, TEST_KIND_RESUME, seal_for_tests};
use super::*;
use crate::crypto::ServerKey;
use crate::payload::{NetCodec, NetGame};
use crate::wire::{WireError, WireReader, WireWriter};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct In(u32);

impl NetCodec for In {
    fn encode(&self, w: &mut WireWriter) {
        w.u32(self.0);
    }
    fn decode(r: &mut WireReader<'_>) -> Result<Self, WireError> {
        Ok(Self(r.u32()?))
    }
}

struct Game;
impl NetGame for Game {
    type Message = Vec<u8>;
    type Input = In;
    type JoinData = ();
    const GAME_ID: &'static str = "test.migration";
    const PROTOCOL_VERSION: u32 = 1;
}

/// SplitMix64 (public domain, S. Vigna): the rig's only randomness.
#[derive(Debug, Clone)]
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next() % n }
    }
    fn chance(&mut self, per_mille: u64) -> bool {
        self.below(1000) < per_mille
    }
}

fn mix(mut h: u64, v: u64) -> u64 {
    h ^= v
        .wrapping_add(0x9e37_79b9_7f4a_7c15)
        .wrapping_add(h << 6)
        .wrapping_add(h >> 2);
    h.wrapping_mul(0x0000_0100_0000_01b3)
}

fn input_of(seat: u16, capture: u64) -> In {
    In((mix(u64::from(seat), capture / 3) & 0xffff) as u32)
}

const STEP_MS: u64 = 4;
const PORT: u16 = 41_000;
const MATCH_ID: u64 = 0x5eed_0027;

/// Datagram impairment on every link (the stream and migration messages are reliable, ordered).
#[derive(Debug, Clone, Copy, Default)]
struct Impair {
    loss_per_mille: u64,
    jitter_ms: u64,
}

#[derive(Debug)]
enum Payload {
    Lockstep {
        lane: Lane,
        bytes: Vec<u8>,
        to_host: bool,
    },
    Migration(Vec<u8>),
}

#[derive(Debug)]
struct Packet {
    at: u64,
    seq: u64,
    from: u16,
    to: u16,
    payload: Payload,
}

struct Proc {
    seat: LockstepSeat<Game>,
    mig: HostMigration,
    host: Option<LockstepHost<Game>>,
    host_of: u16,
    alive: bool,
    key: ServerKey,
    listening: Option<u16>,
    state: u64,
    /// The first tick this process stepped (0, or after a catch-up's state).
    first_tick: u64,
    stepped: Vec<u64>,
    hashes: BTreeMap<u64, u64>,
    events: Vec<(u64, MigrationEvent)>,
    refused: Vec<&'static str>,
    /// `(port, winner)` of every connection made to a re-host listener.
    connected_on: Vec<(u16, u16)>,
    /// `(ms, seat, present)` of seat 0's entry in the frames this process stepped.
    seat0_entries: Vec<(u64, bool)>,
}

struct World {
    now: u64,
    seq: u64,
    rng: Rng,
    impair: Impair,
    access: BTreeMap<u16, u64>,
    procs: BTreeMap<u16, Proc>,
    flying: Vec<Packet>,
    /// Port to the static key of whatever listens on it.
    ports: BTreeMap<u16, (ServerKey, Option<u16>)>,
    stream_clock: BTreeMap<(u16, u16), u64>,
    /// Lockstep payloads from the host to this process are lost until the time given; the
    /// transport's keepalives still arrive.
    payloads_lost_until: BTreeMap<u16, u64>,
    config: LockstepConfig,
    mconfig: MigrationConfig,
    roster: MigrationRoster,
    tokens: BTreeMap<u16, u64>,
}

fn key_of(seat: u16) -> ServerKey {
    ServerKey([seat as u8 + 1; 32])
}

fn roster(n: u16) -> MigrationRoster {
    MigrationRoster {
        match_id: MATCH_ID,
        secret: [0x27; 32],
        port: PORT,
        host_seat: Some(SeatId(0)),
        seats: (0..n)
            .map(|i| RosterSeat {
                seat: SeatId(i),
                key: key_of(i),
                ip: Some(IpAddr::V4(Ipv4Addr::new(10, 0, 0, i as u8 + 1))),
                generation: 0,
            })
            .collect(),
    }
}

fn lockstep_config() -> LockstepConfig {
    LockstepConfig::default()
}

impl World {
    /// `n` processes; process 0 hosts (a relay host with its own seat on an in-process link).
    /// `access[i]` is process i's one-way access latency: a datagram from i to j takes
    /// `access[i] + access[j]`.
    fn new(access: &[u64], impair: Impair, seed: u64, mconfig: MigrationConfig) -> Self {
        let n = access.len() as u16;
        let config = lockstep_config();
        let roster = roster(n);
        let mut host = LockstepHost::<Game>::new(config.clone(), HostRole::Relay);
        for i in 0..n {
            host.add_seat(SeatId(i));
        }
        host.start(0);
        host.share_roster(roster.clone());
        let tokens = (0..n)
            .map(|i| (i, host.resume_token(SeatId(i)).expect("a seat of the match")))
            .collect();
        let mut procs = BTreeMap::new();
        let mut host = Some(host);
        for i in 0..n {
            procs.insert(
                i,
                Proc {
                    seat: LockstepSeat::new(config.clone(), SeatId(i)),
                    mig: HostMigration::new(SeatId(i), mconfig.clone(), config.clone()),
                    host: if i == 0 { host.take() } else { None },
                    host_of: 0,
                    alive: true,
                    key: key_of(i),
                    listening: None,
                    state: 0,
                    first_tick: 0,
                    stepped: Vec::new(),
                    hashes: BTreeMap::new(),
                    events: Vec::new(),
                    refused: Vec::new(),
                    connected_on: Vec::new(),
                    seat0_entries: Vec::new(),
                },
            );
        }
        Self {
            now: 0,
            seq: 0,
            rng: Rng(seed),
            impair,
            access: access.iter().enumerate().map(|(i, a)| (i as u16, *a)).collect(),
            procs,
            flying: Vec::new(),
            ports: BTreeMap::new(),
            stream_clock: BTreeMap::new(),
            payloads_lost_until: BTreeMap::new(),
            config,
            mconfig,
            roster,
            tokens,
        }
    }

    fn latency(&self, a: u16, b: u16) -> u64 {
        if a == b {
            return 0;
        }
        self.access.get(&a).copied().unwrap_or(10) + self.access.get(&b).copied().unwrap_or(10)
    }

    fn send(&mut self, from: u16, to: u16, payload: Payload) {
        let lat = self.latency(from, to);
        let reliable = match &payload {
            Payload::Lockstep { lane, to_host, .. } => {
                if !*to_host
                    && from != to
                    && self
                        .payloads_lost_until
                        .get(&to)
                        .is_some_and(|until| self.now < *until)
                {
                    return;
                }
                *lane == Lane::Stream
            }
            Payload::Migration(_) => true,
        };
        let at = if from == to {
            self.now
        } else if reliable {
            let clock = self.stream_clock.entry((from, to)).or_insert(0);
            *clock = (*clock).max(self.now + lat);
            *clock
        } else {
            if self.rng.chance(self.impair.loss_per_mille) {
                return;
            }
            self.now + lat + self.rng.below(self.impair.jitter_ms + 1)
        };
        self.seq += 1;
        self.flying.push(Packet {
            at,
            seq: self.seq,
            from,
            to,
            payload,
        });
    }

    fn deliver(&mut self) {
        let now = self.now;
        let mut due: Vec<Packet> = Vec::new();
        let mut i = 0;
        while i < self.flying.len() {
            if self.flying[i].at <= now {
                due.push(self.flying.swap_remove(i));
            } else {
                i += 1;
            }
        }
        due.sort_by_key(|p| (p.at, p.seq));
        for packet in due {
            let Some(p) = self.procs.get_mut(&packet.to) else {
                continue;
            };
            if !p.alive {
                continue;
            }
            match packet.payload {
                Payload::Lockstep {
                    lane,
                    bytes,
                    to_host: true,
                } => {
                    if let Some(host) = &mut p.host {
                        let _ = host.receive(SeatId(packet.from), lane, &bytes, now);
                    }
                }
                Payload::Lockstep {
                    lane,
                    bytes,
                    to_host: false,
                } => {
                    // A seat takes lockstep traffic from the host it is connected to only: the
                    // one it knew, or the one a resume message just named (the new host's first
                    // stream payloads follow the resume on the same link, in the same delivery).
                    let current = p.mig.current_host().map(|s| s.0);
                    if packet.from == p.host_of || (p.mig.migrations() > 0 && current == Some(packet.from)) {
                        let _ = p.seat.receive(lane, &bytes, now);
                    }
                }
                Payload::Migration(bytes) => {
                    if let Err(e) = p.mig.on_message(&mut p.seat, &bytes, now) {
                        p.refused.push(e.code());
                    }
                }
            }
        }
    }

    fn step(&mut self) {
        self.deliver();
        let ids: Vec<u16> = self.procs.keys().copied().collect();
        for id in ids {
            let Some(mut p) = self.procs.remove(&id) else {
                continue;
            };
            if p.alive {
                self.run(id, &mut p);
            }
            self.procs.insert(id, p);
        }
        self.now += STEP_MS;
    }

    fn run(&mut self, id: u16, p: &mut Proc) {
        let now = self.now;
        // The transport still hears a host whose lockstep payloads are being lost.
        if self
            .payloads_lost_until
            .get(&id)
            .is_some_and(|until| now < *until)
            && self.procs.get(&p.host_of).is_some_and(|h| h.alive)
        {
            p.seat.note_host_heard(now);
        }
        p.mig.poll(&mut p.seat, now);
        self.migration_io(id, p);

        if let Some(host) = &mut p.host {
            if now.is_multiple_of(500) {
                let seats: Vec<u16> = self.access.keys().copied().collect();
                for s in seats {
                    let rtt = 2 * self.latency(id, s);
                    host.note_transport_rtt(SeatId(s), rtt);
                }
            }
            while host.poll_frame(now).is_some() {}
            let out = host.outgoing(now);
            for (to, o) in out {
                self.send(
                    id,
                    to.0,
                    Payload::Lockstep {
                        lane: o.lane,
                        bytes: o.bytes,
                        to_host: false,
                    },
                );
            }
        }

        while let Some(event) = p.seat.poll_event() {
            if let SeatEvent::StateReady { .. } = event
                && let Some((tick, bytes)) = p.seat.take_state()
            {
                let mut word = [0u8; 8];
                word.copy_from_slice(&bytes[..8]);
                p.state = u64::from_le_bytes(word);
                p.first_tick = tick + 1;
                p.stepped.clear();
            }
        }
        let _ = p.seat.submit_input(&input_of(id, p.seat.tick()));
        while let Some(frame) = p.seat.next_frame() {
            p.state = mix(p.state, frame.tick);
            for (seat, input) in &frame.inputs {
                p.state = mix(p.state, u64::from(seat.0));
                p.state = mix(p.state, input.as_ref().map_or(0xab5e, |i| u64::from(i.0) + 1));
                if seat.0 == 0 {
                    p.seat0_entries.push((frame.tick, input.is_present()));
                }
            }
            p.stepped.push(frame.tick);
            p.hashes.insert(frame.tick, p.state);
            let _ = p.seat.submit_input(&input_of(id, p.seat.tick()));
        }
        let host_of = p.host_of;
        for o in p.seat.outgoing(now) {
            self.send(
                id,
                host_of,
                Payload::Lockstep {
                    lane: o.lane,
                    bytes: o.bytes,
                    to_host: true,
                },
            );
        }
    }

    fn migration_io(&mut self, id: u16, p: &mut Proc) {
        let now = self.now;
        loop {
            let mut progressed = false;
            while let Some(action) = p.mig.poll_action() {
                progressed = true;
                match action {
                    MigrationAction::Listen { ports } => {
                        match ports.iter().copied().find(|port| !self.ports.contains_key(port)) {
                            Some(port) => {
                                self.ports.insert(port, (p.key, Some(id)));
                                p.listening = Some(port);
                                p.mig.listening(port);
                            }
                            None => p.mig.listen_failed(&p.seat, now),
                        }
                    }
                    MigrationAction::StopListening => {
                        if let Some(port) = p.listening.take() {
                            self.ports.remove(&port);
                        }
                    }
                    MigrationAction::Connect { seat, addrs, key } => {
                        // Each address is tried in order with `key` pinned: a listener that proves
                        // another key fails the handshake and the next address is tried.
                        let reached = addrs.iter().find_map(|addr| {
                            let (listener_key, owner) = self.ports.get(&addr.port())?;
                            let owner = (*owner)?;
                            let alive = self.procs.get(&owner).is_some_and(|o| o.alive);
                            (*listener_key == key && alive).then_some(addr.port())
                        });
                        match reached {
                            Some(port) => {
                                p.connected_on.push((port, seat.0));
                                p.mig.connected(seat);
                            }
                            None => p.mig.connect_failed(seat),
                        }
                    }
                    MigrationAction::Send { to, bytes } => self.send(id, to.0, Payload::Migration(bytes)),
                }
            }
            while let Some(event) = p.mig.poll_event() {
                progressed = true;
                p.events.push((now, event));
                match event {
                    MigrationEvent::Promoted { .. } => {
                        let host = p
                            .mig
                            .take_host::<Game>()
                            .expect("a promoted seat has a host")
                            .expect("the resumed host builds");
                        p.host = Some(host);
                        p.host_of = id;
                        if let Some(port) = p.listening.take() {
                            // The game would keep its listener as the new host's transport.
                            let _ = port;
                        }
                    }
                    MigrationEvent::Resuming { .. } => {
                        if let Some(host) = p.mig.current_host() {
                            p.host_of = host.0;
                        }
                    }
                    _ => {}
                }
            }
            if !progressed {
                break;
            }
        }
    }

    fn run_until(&mut self, ms: u64) {
        while self.now < ms {
            self.step();
        }
    }

    fn kill(&mut self, id: u16) {
        if let Some(p) = self.procs.get_mut(&id) {
            p.alive = false;
        }
        self.ports.retain(|_, (_, owner)| *owner != Some(id));
    }

    fn alive(&self) -> impl Iterator<Item = (&u16, &Proc)> {
        self.procs.iter().filter(|(_, p)| p.alive)
    }

    fn next_ticks(&self) -> BTreeMap<u16, u64> {
        self.alive()
            .map(|(id, p)| (*id, p.seat.frames_received_through()))
            .collect()
    }

    /// Every live process stepped each tick once, in order, and every one of them computed the same
    /// state at every tick they all stepped.
    fn assert_in_step(&self) {
        for (id, p) in self.alive() {
            assert!(
                p.stepped
                    .iter()
                    .enumerate()
                    .all(|(i, t)| *t == p.first_tick + i as u64),
                "process {id} stepped out of order or twice: first {} then {:?}",
                p.first_tick,
                p.stepped
                    .windows(2)
                    .find(|w| w[1] != w[0] + 1)
                    .map(|w| (w[0], w[1]))
            );
        }
        let mut reference: BTreeMap<u64, (u16, u64)> = BTreeMap::new();
        for (id, p) in self.alive() {
            for (tick, hash) in &p.hashes {
                if *tick < p.first_tick {
                    continue;
                }
                match reference.get(tick) {
                    Some((other, h)) => {
                        assert_eq!(h, hash, "processes {other} and {id} diverged at tick {tick}")
                    }
                    None => {
                        reference.insert(*tick, (*id, *hash));
                    }
                }
            }
        }
    }

    fn events_of(&self, id: u16) -> Vec<MigrationEvent> {
        self.procs[&id].events.iter().map(|(_, e)| *e).collect()
    }

    /// The highest next tick any survivor needed when it saw the host lost (the frames the host
    /// sent before it died, in flight then, included): the tick the match must resume at.
    fn highest_lost(&self) -> u64 {
        self.alive()
            .filter_map(|(_, p)| {
                p.events.iter().rev().find_map(|(_, e)| match e {
                    MigrationEvent::HostLost { tick } => Some(*tick),
                    _ => None,
                })
            })
            .max()
            .unwrap_or(0)
    }

    fn host_seat(&self) -> Option<u16> {
        self.alive().find(|(_, p)| p.host.is_some()).map(|(id, _)| *id)
    }
}

fn resuming_tick(events: &[MigrationEvent]) -> Option<u64> {
    events.iter().rev().find_map(|e| match e {
        MigrationEvent::Resuming { tick, .. } => Some(*tick),
        _ => None,
    })
}

/// Runs a match, kills the host at `kill_ms`, and checks that the match continued on the
/// winner: the readout, the resume tick (the highest next tick of the survivors), and that every
/// survivor kept stepping in step for `after_ms`.
fn host_drop(access: &[u64], kill_ms: u64, impair: Impair, seed: u64) -> World {
    let mut w = World::new(access, impair, seed, MigrationConfig::default());
    w.run_until(kill_ms);
    let before = w.next_ticks();
    w.kill(0);
    let highest = before
        .iter()
        .filter(|(id, _)| **id != 0)
        .map(|(_, t)| *t)
        .max()
        .expect("survivors");
    let kill_tick = before[&0];
    w.run_until(kill_ms + 12_000);
    assert!(w.highest_lost() >= highest, "frames only arrive after the kill");
    let highest = w.highest_lost();
    let winner = w.host_seat().expect("a survivor hosts the match");
    assert_ne!(winner, 0);
    for (id, p) in w.alive() {
        let events: Vec<MigrationEvent> = p.events.iter().map(|(_, e)| *e).collect();
        assert!(
            matches!(events.first(), Some(MigrationEvent::HostLost { .. })),
            "seat {id} saw {events:?}"
        );
        assert!(
            events.contains(&MigrationEvent::Electing {
                winner: SeatId(winner),
                rank: 0
            }),
            "seat {id} saw {events:?}"
        );
        assert_eq!(
            resuming_tick(&events),
            Some(highest),
            "seat {id}: the match resumes at the highest next tick ({highest}; host died at {kill_tick})"
        );
        assert!(
            p.stepped.last().copied().unwrap_or(0) > highest + 300,
            "seat {id} kept stepping after the migration: last {:?}",
            p.stepped.last()
        );
    }
    w.assert_in_step();
    w
}

#[test]
fn the_host_drops_at_several_ticks_with_3_and_8_peers_and_the_match_continues() {
    for access in [&[5u64, 30, 12][..], &[5, 40, 22, 9, 31, 17, 26, 44][..]] {
        for (k, kill_ms) in [2_011u64, 5_303, 9_877].into_iter().enumerate() {
            let w = host_drop(access, kill_ms, Impair::default(), 11 + k as u64);
            // The best connected survivor wins: the lowest mean round trip (ties by seat id).
            let survivors: Vec<u16> = (1..access.len() as u16).collect();
            let best = *survivors
                .iter()
                .min_by_key(|s| {
                    (
                        survivors
                            .iter()
                            .filter(|o| *o != *s)
                            .map(|o| access[usize::from(*o)] + access[usize::from(**s)])
                            .sum::<u64>(),
                        **s,
                    )
                })
                .expect("survivors");
            assert_eq!(w.host_seat(), Some(best), "peers {} kill {kill_ms}", access.len());
            // HOST LOST -> Electing -> Resuming { in_ticks } (the countdown of resume_lead_ms).
            let events = w.events_of(best);
            assert!(events.contains(&MigrationEvent::Resuming {
                tick: resuming_tick(&events).expect("resumed"),
                in_ticks: 60,
            }));
        }
    }
}

#[test]
fn the_winner_dies_too_and_the_next_ranked_takes_over_after_3_s() {
    // Access latencies rank seat 3, then seat 1, then seat 2.
    let access = [5u64, 20, 35, 8];
    let mut w = World::new(&access, Impair::default(), 21, MigrationConfig::default());
    w.run_until(4_000);
    w.kill(0);
    w.kill(3);
    let highest = w
        .next_ticks()
        .iter()
        .filter(|(id, _)| **id != 0 && **id != 3)
        .map(|(_, t)| *t)
        .max()
        .expect("survivors");
    w.run_until(20_000);
    assert!(w.highest_lost() >= highest);
    let highest = w.highest_lost();
    assert_eq!(w.host_seat(), Some(1), "the next ranked hosts");
    for id in [1u16, 2] {
        let events = w.events_of(id);
        assert!(events.contains(&MigrationEvent::Electing {
            winner: SeatId(3),
            rank: 0
        }));
        assert!(
            events.contains(&MigrationEvent::Electing {
                winner: SeatId(1),
                rank: 1
            }),
            "{events:?}"
        );
        assert_eq!(resuming_tick(&events), Some(highest));
        let elect_3 = w.procs[&id]
            .events
            .iter()
            .find(|(_, e)| {
                matches!(
                    e,
                    MigrationEvent::Electing {
                        winner: SeatId(3),
                        ..
                    }
                )
            })
            .map(|(ms, _)| *ms)
            .expect("elected 3");
        let elect_1 = w.procs[&id]
            .events
            .iter()
            .find(|(_, e)| {
                matches!(
                    e,
                    MigrationEvent::Electing {
                        winner: SeatId(1),
                        ..
                    }
                )
            })
            .map(|(ms, _)| *ms)
            .expect("elected 1");
        assert!(
            elect_1 - elect_3 >= 3_000 && elect_1 - elect_3 < 3_100,
            "{elect_3} -> {elect_1}"
        );
    }
    w.assert_in_step();
}

#[test]
fn peers_at_unequal_ticks_resume_at_the_highest_and_catch_ups_pass_the_stream_cap() {
    // The winner (seat 1, best connected) gets no lockstep payload for the last 6 s before the
    // host dies: it lacks 360 frames that the others send it in hello parts (more than the 256
    // frames one payload carries). Seat 3 lacks 7.5 s of frames: the new host sends it 450 on
    // the stream, in batches.
    let access = [5u64, 6, 20, 30];
    let mconfig = MigrationConfig {
        migrate_after_ms: 3_000,
        ..MigrationConfig::default()
    };
    let mut w = World::new(&access, Impair::default(), 31, mconfig);
    w.run_until(4_000);
    w.payloads_lost_until.insert(1, 30_000);
    w.run_until(4_000 + 1_500);
    w.payloads_lost_until.insert(3, 30_000);
    w.run_until(12_000);
    let before = w.next_ticks();
    w.kill(0);
    let highest = before[&2];
    assert!(highest >= before[&1] + 300, "{before:?}");
    assert!(highest >= before[&3] + 256, "{before:?}");
    w.payloads_lost_until.clear();
    w.run_until(26_000);
    assert!(w.highest_lost() >= highest);
    let highest = w.highest_lost();
    assert_eq!(w.host_seat(), Some(1));
    for id in 1..4u16 {
        assert_eq!(
            resuming_tick(&w.events_of(id)),
            Some(highest),
            "seat {id}: {before:?}"
        );
        let p = &w.procs[&id];
        assert!(
            p.stepped.last().copied().unwrap_or(0) > highest + 300,
            "seat {id} stepped to {:?}, received {}, cache {:?}, host tick {:?}, events {:?}, status {:?}",
            p.stepped.last(),
            p.seat.frames_received_through(),
            p.seat.frame_cache(),
            w.procs[&1].host.as_ref().map(|h| h.tick()),
            p.events,
            p.seat.status()
        );
    }
    w.assert_in_step();
}

#[test]
fn the_agreed_port_is_taken_and_the_winner_listens_on_the_next_one() {
    let access = [5u64, 6, 20];
    let mut w = World::new(&access, Impair::default(), 41, MigrationConfig::default());
    // Another program holds the agreed port: it proves a key that is no seat's.
    w.ports.insert(PORT, (ServerKey([0xee; 32]), None));
    w.run_until(3_000);
    w.kill(0);
    w.run_until(14_000);
    assert_eq!(w.host_seat(), Some(1));
    assert_eq!(w.procs[&1].listening, None, "the listener became the host's link");
    assert_eq!(w.procs[&2].connected_on, vec![(PORT + 1, 1)]);
    w.assert_in_step();
}

#[test]
fn an_impostor_on_the_next_port_is_skipped_by_the_pinned_key() {
    let access = [5u64, 6, 20];
    let mut w = World::new(&access, Impair::default(), 42, MigrationConfig::default());
    // Seat 1 cannot take the agreed port; an impostor that claims the winner's address sits on
    // the next one. The seats pin seat 1's static key, so they skip it.
    w.ports.insert(PORT, (ServerKey([0xee; 32]), None));
    w.ports.insert(PORT + 1, (ServerKey([0xdd; 32]), None));
    w.run_until(3_000);
    w.kill(0);
    w.run_until(14_000);
    assert_eq!(w.host_seat(), Some(1));
    assert_eq!(w.procs[&2].connected_on, vec![(PORT + 2, 1)]);
    w.assert_in_step();
}

#[test]
fn the_old_host_rejoins_the_migrated_match_as_a_player() {
    let access = [5u64, 6, 20];
    let mut w = World::new(&access, Impair::default(), 51, MigrationConfig::default());
    w.run_until(3_000);
    w.kill(0);
    w.run_until(12_000);
    assert_eq!(w.host_seat(), Some(1));
    // The old host's process comes back as a player: its resume token (keyed by the match's
    // migration secret, so any host of the match accepts it) gives it back seat 0.
    let token = w.tokens[&0];
    let now = w.now;
    let (state_tick, state) = {
        let winner = &w.procs[&1];
        let tick = *winner.hashes.keys().next_back().expect("stepped");
        (tick, winner.hashes[&tick])
    };
    {
        let host = w.procs.get_mut(&1).and_then(|p| p.host.as_mut()).expect("host");
        assert_eq!(host.resume(token, now), Ok(SeatId(0)));
        host.catch_up(SeatId(0), state_tick, &state.to_le_bytes(), now)
            .expect("the frame log holds the frames after the state");
    }
    let config = w.config.clone();
    let mconfig = w.mconfig.clone();
    let p = w.procs.get_mut(&0).expect("process 0");
    p.seat = LockstepSeat::catching_up(config.clone(), SeatId(0));
    p.mig = HostMigration::new(SeatId(0), mconfig, config);
    p.host = None;
    p.host_of = 1;
    p.alive = true;
    p.stepped.clear();
    p.hashes.clear();
    p.seat0_entries.clear();
    w.run_until(20_000);
    let p = &w.procs[&0];
    assert!(
        p.first_tick > 0 && p.stepped.len() > 300,
        "the old host caught up and steps"
    );
    assert!(
        p.seat0_entries
            .iter()
            .rev()
            .take(60)
            .filter(|(_, present)| *present)
            .count()
            > 50,
        "its inputs are in the frames again"
    );
    assert!(p.mig.status().roster, "the new host shares the roster with it");
    w.assert_in_step();
}

#[test]
fn the_migration_completes_under_reorder_and_loss() {
    let impair = Impair {
        loss_per_mille: 30,
        jitter_ms: 30,
    };
    for (seed, access) in [
        (61u64, &[5u64, 30, 12][..]),
        (62, &[5, 40, 22, 9, 31, 17, 26, 44][..]),
    ] {
        let w = host_drop(access, 6_007, impair, seed);
        assert!(w.host_seat().is_some());
    }
}

#[test]
fn two_migrations_in_a_row() {
    let access = [5u64, 6, 20, 9];
    let mut w = World::new(&access, Impair::default(), 71, MigrationConfig::default());
    w.run_until(3_000);
    w.kill(0);
    w.run_until(12_000);
    let first = w.host_seat().expect("first migration");
    assert_eq!(first, 1);
    // The new host shares a matrix of its own round trips; then it quits too.
    w.run_until(16_000);
    let before = w.next_ticks();
    w.kill(first);
    let highest = before
        .iter()
        .filter(|(id, _)| **id != first)
        .map(|(_, t)| *t)
        .max()
        .expect("survivors");
    w.run_until(30_000);
    assert!(w.highest_lost() >= highest);
    let highest = w.highest_lost();
    let second = w.host_seat().expect("second migration");
    assert_ne!(second, first);
    for (id, p) in w.alive() {
        assert_eq!(p.mig.migrations(), 2, "seat {id}");
        assert_eq!(p.mig.status().epoch, 2);
        assert_eq!(resuming_tick(&w.events_of(*id)), Some(highest));
        assert!(p.stepped.last().copied().unwrap_or(0) > highest + 300);
    }
    w.assert_in_step();
}

#[test]
fn a_non_member_can_neither_win_nor_join_the_election() {
    let access = [5u64, 6, 20, 30];
    let mut w = World::new(&access, Impair::default(), 81, MigrationConfig::default());
    w.run_until(3_000);
    w.kill(0);
    // Run until seat 2 is electing: seat 1 leads (it lost the host first), seat 2 follows it.
    while !w.procs[&2].mig.electing() {
        assert!(w.now < 10_000, "seat 2 never noticed the host is gone");
        w.step();
    }
    assert_eq!(w.procs[&1].mig.candidate(), Some(SeatId(1)), "seat 1 leads");
    let foreign = MigrationRoster {
        secret: [0x99; 32],
        ..w.roster.clone()
    };
    let now = w.now;
    {
        // A forged answer naming the impostor as the winner with a newer matrix, without the
        // migration secret: refused, seat 2 keeps following seat 1.
        let p = w.procs.get_mut(&2).expect("seat 2");
        let forged = seal_for_tests(&foreign, 0, SeatId(3), TEST_KIND_ELECT, 0);
        let err = p
            .mig
            .on_message(&mut p.seat, &forged, now)
            .expect_err("forged election");
        assert_eq!(err.code(), "migration_bad_mac");
        // A process with the secret but no seat of the roster (seat 99) is no member.
        let outsider = seal_for_tests(&w.roster, 0, SeatId(99), TEST_KIND_ELECT, 0);
        let err = p
            .mig
            .on_message(&mut p.seat, &outsider, now)
            .expect_err("non-member");
        assert_eq!(err.code(), "migration_not_member");
        // A resume from an earlier migration does not apply to this one.
        let stale = seal_for_tests(&w.roster, 7, SeatId(1), TEST_KIND_RESUME, 1);
        assert_eq!(
            p.mig.on_message(&mut p.seat, &stale, now).map_err(|e| e.code()),
            Err("migration_stale_epoch")
        );
        assert_eq!(p.mig.candidate(), Some(SeatId(1)));
    }
    {
        // At the winner: a claim and a hello from outside the match never get frames or a seat.
        let p = w.procs.get_mut(&1).expect("seat 1");
        for (kind, roster, from) in [
            (TEST_KIND_CLAIM, &foreign, SeatId(2)),
            (TEST_KIND_CLAIM, &w.roster, SeatId(99)),
            (TEST_KIND_HELLO, &foreign, SeatId(3)),
        ] {
            let bytes = seal_for_tests(roster, 0, from, kind, 0);
            assert!(p.mig.on_message(&mut p.seat, &bytes, now).is_err());
        }
        assert!(p.mig.status().refused >= 3);
        // A truncated message is malformed.
        assert_eq!(
            p.mig
                .on_message(&mut p.seat, &[0x30, 1, 2], now)
                .map_err(|e| e.code()),
            Err("migration_malformed")
        );
    }
    w.run_until(16_000);
    assert_eq!(w.host_seat(), Some(1));
    // The resumed host lists exactly the seats of the match.
    let members: BTreeSet<SeatId> = w.procs[&1].host.as_ref().expect("host").members().collect();
    assert_eq!(members, (0..4).map(SeatId).collect());
    w.assert_in_step();
}

#[test]
fn every_seat_ranks_the_survivors_the_same_way() {
    let mut matrix = RttMatrix {
        version: 3,
        ..RttMatrix::default()
    };
    for (seat, rtt) in [(1u16, Some(40u16)), (2, Some(10)), (3, Some(10)), (4, None)] {
        matrix.host_rtt_ms.insert(SeatId(seat), rtt);
    }
    let survivors: BTreeSet<SeatId> = (1..5).map(SeatId).collect();
    // Seats 2 and 3 tie on round trips: the lower id first; an unmeasured seat ranks last.
    assert_eq!(
        rank_survivors(&matrix, &survivors),
        vec![SeatId(2), SeatId(3), SeatId(1), SeatId(4)]
    );
    // A measured round trip between two seats replaces the estimate through the host.
    matrix.pair_rtt_ms.insert((SeatId(1), SeatId(2)), 5);
    matrix.pair_rtt_ms.insert((SeatId(1), SeatId(3)), 5);
    assert_eq!(rank_survivors(&matrix, &survivors)[0], SeatId(1));
    // The order does not depend on the order seats were inserted.
    let reversed: BTreeSet<SeatId> = survivors.iter().rev().copied().collect();
    assert_eq!(
        rank_survivors(&matrix, &reversed),
        rank_survivors(&matrix, &survivors)
    );
}

#[test]
fn the_roster_and_matrix_round_trip_and_the_roster_debug_hides_the_secret() {
    let r = roster(3);
    let bytes = super::migration::encode_roster(&r);
    assert_eq!(super::migration::decode_roster(&bytes), Ok(r.clone()));
    assert!(!format!("{r:?}").contains("39, 39, 39"), "{r:?}");
    assert!(super::migration::decode_roster(&bytes[..bytes.len() - 1]).is_err());
    let mut m = RttMatrix {
        version: 9,
        ..RttMatrix::default()
    };
    m.host_rtt_ms.insert(SeatId(1), Some(12));
    m.host_rtt_ms.insert(SeatId(2), None);
    m.pair_rtt_ms.insert((SeatId(1), SeatId(2)), 7);
    assert_eq!(
        super::migration::decode_rtt(&super::migration::encode_rtt(&m)),
        Ok(m)
    );
    assert_eq!(listen_ports(65_530).len(), 15, "port 0 is skipped");
}

#[test]
fn resume_tokens_under_the_migration_secret_are_the_same_on_every_host() {
    let mut a = LockstepHost::<Game>::new(lockstep_config(), HostRole::Relay);
    let mut b = LockstepHost::<Game>::new(lockstep_config(), HostRole::Relay);
    a.add_seat(SeatId(4));
    b.add_seat(SeatId(4));
    // Without a roster each host keys tokens with its own secret.
    assert_ne!(a.resume_token(SeatId(4)), b.resume_token(SeatId(4)));
    a.share_roster(roster(5));
    b.share_roster(roster(5));
    assert_eq!(a.resume_token(SeatId(4)), b.resume_token(SeatId(4)));
}

#[test]
fn the_seat_cache_holds_the_last_frames_with_their_base() {
    let access = [5u64, 6];
    let mut w = World::new(&access, Impair::default(), 91, MigrationConfig::default());
    w.run_until(15_000);
    let p = &w.procs[&1];
    let (front, len) = p.seat.frame_cache();
    assert_eq!(len, 600, "600 frames, 10 s at 60 Hz");
    assert_eq!(front + len as u64, p.seat.frames_received_through());
    // The bodies decode from the cache's base: the chain is whole.
    let (mut base, front2, bodies) = p.seat.cache_parts();
    assert_eq!(front2, front);
    for (i, body) in bodies.iter().enumerate() {
        super::wire::decode_frame_body(body, front + i as u64, &mut base, 1024).expect("decodes");
    }
}
