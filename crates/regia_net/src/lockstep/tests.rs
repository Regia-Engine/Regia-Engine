// SPDX-License-Identifier: MPL-2.0
//! Lockstep core tests: a host and seats on virtual-time links (seeded; no wall clock, no sleeps).

use std::collections::{BTreeMap, BTreeSet};

use super::wire::{self, FrameState, RawFrame};
use super::*;
use crate::payload::{NetCodec, NetGame};
use crate::wire::{WireError, WireReader, WireWriter};

/// A 12-byte input shaped like an action game's: two axes, aim and buttons.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct Pad {
    move_x: i8,
    move_y: i8,
    aim: u32,
    buttons: u16,
    blob: Vec<u8>,
}

impl NetCodec for Pad {
    fn encode(&self, w: &mut WireWriter) {
        w.u8(self.move_x as u8);
        w.u8(self.move_y as u8);
        w.u32(self.aim);
        w.u16(self.buttons);
        if let Err(e) = w.bytes_u16(&self.blob, 4096) {
            w.fail(e);
        }
    }
    fn decode(r: &mut WireReader<'_>) -> Result<Self, WireError> {
        Ok(Self {
            move_x: r.u8()? as i8,
            move_y: r.u8()? as i8,
            aim: r.u32()?,
            buttons: r.u16()?,
            blob: r.bytes_u16(4096)?.to_vec(),
        })
    }
}

struct Game;
impl NetGame for Game {
    type Message = Vec<u8>;
    type Input = Pad;
    type JoinData = ();
    const GAME_ID: &'static str = "test.lockstep";
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

/// A deterministic toy simulation: its whole state is one word folded from every frame.
#[derive(Debug, Clone, Default)]
struct Sim {
    state: u64,
    stepped: Vec<u64>,
    /// State after each checkpoint tick (and before tick 0 under `u64::MAX`).
    snapshots: BTreeMap<u64, u64>,
    hashes: BTreeMap<u64, u64>,
}

impl Sim {
    fn step(&mut self, frame: &LockstepFrame<Pad>, hash_every: u64) {
        self.stepped.push(frame.tick);
        self.state = Self::fold(self.state, frame);
        self.hashes.insert(frame.tick, self.state);
        if hash_every > 0 && frame.tick.is_multiple_of(hash_every) {
            self.snapshots.insert(frame.tick, self.state);
        }
    }

    fn fold(mut state: u64, frame: &LockstepFrame<Pad>) -> u64 {
        state = mix(state, frame.tick);
        for (seat, input) in &frame.inputs {
            state = mix(state, u64::from(seat.0));
            if let SeatInput::Present(p) = input {
                state = mix(state, p.move_x as u64);
                state = mix(state, p.move_y as u64);
                state = mix(state, u64::from(p.aim));
                state = mix(state, u64::from(p.buttons));
                for b in &p.blob {
                    state = mix(state, u64::from(*b));
                }
            } else {
                state = mix(state, 0x00ab_5e17);
            }
        }
        state
    }

    /// Replays `frames` on top of the state after `from` (`None`: the start).
    fn replay<'a>(&mut self, from: Option<u64>, frames: impl Iterator<Item = &'a LockstepFrame<Pad>>) {
        let mut state = from.map_or(0, |t| self.snapshots.get(&t).copied().unwrap_or(0));
        for frame in frames {
            state = Self::fold(state, frame);
            self.hashes.insert(frame.tick, state);
        }
        self.state = state;
    }
}

#[derive(Debug, Clone, Copy)]
struct LinkSpec {
    latency_ms: u64,
    jitter_ms: u64,
    loss_per_mille: u64,
    /// Chance per datagram of starting a burst, and of a burst continuing.
    burst_start_per_mille: u64,
    burst_stay_per_mille: u64,
    dup_per_mille: u64,
}

impl LinkSpec {
    const CLEAN: Self = Self {
        latency_ms: 1,
        jitter_ms: 0,
        loss_per_mille: 0,
        burst_start_per_mille: 0,
        burst_stay_per_mille: 0,
        dup_per_mille: 0,
    };
    fn latency(ms: u64) -> Self {
        Self {
            latency_ms: ms,
            ..Self::CLEAN
        }
    }
    /// 8 ms + 0-30 ms jitter, 1 % loss plus bursts, 0.5 % duplicates: the soak kit's bad-wifi.
    const BAD_WIFI: Self = Self {
        latency_ms: 8,
        jitter_ms: 30,
        loss_per_mille: 10,
        burst_start_per_mille: 10,
        burst_stay_per_mille: 420,
        dup_per_mille: 5,
    };
}

#[derive(Debug)]
struct Pipe {
    spec: LinkSpec,
    rng: Rng,
    in_burst: bool,
    /// `(deliver ms, lane, bytes)`.
    flying: Vec<(u64, Lane, Vec<u8>)>,
    stream_at: u64,
    drop_next: u32,
    sent: u64,
    sent_bytes: u64,
    dropped: u64,
}

impl Pipe {
    fn new(spec: LinkSpec, seed: u64) -> Self {
        Self {
            spec,
            rng: Rng(seed),
            in_burst: false,
            flying: Vec::new(),
            stream_at: 0,
            drop_next: 0,
            sent: 0,
            sent_bytes: 0,
            dropped: 0,
        }
    }

    fn send(&mut self, now: u64, out: Outgoing) {
        let jitter = self.rng.below(self.spec.jitter_ms + 1);
        let at = now + self.spec.latency_ms + jitter;
        match out.lane {
            Lane::Stream => {
                // Reliable and ordered: never lost, never overtaken.
                self.stream_at = self.stream_at.max(at);
                self.flying.push((self.stream_at, Lane::Stream, out.bytes));
            }
            Lane::Datagram => {
                self.sent += 1;
                self.sent_bytes += out.bytes.len() as u64;
                self.in_burst = if self.in_burst {
                    self.rng.chance(self.spec.burst_stay_per_mille)
                } else {
                    self.rng.chance(self.spec.burst_start_per_mille)
                };
                let lost = self.in_burst || self.rng.chance(self.spec.loss_per_mille);
                if self.drop_next > 0 {
                    self.drop_next -= 1;
                    self.dropped += 1;
                    return;
                }
                if lost {
                    self.dropped += 1;
                    return;
                }
                if self.rng.chance(self.spec.dup_per_mille) {
                    self.flying.push((at + 3, Lane::Datagram, out.bytes.clone()));
                }
                self.flying.push((at, Lane::Datagram, out.bytes));
            }
        }
    }

    fn due(&mut self, now: u64) -> Vec<(Lane, Vec<u8>)> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < self.flying.len() {
            if self.flying[i].0 <= now {
                let (_, lane, bytes) = self.flying.remove(i);
                out.push((lane, bytes));
            } else {
                i += 1;
            }
        }
        out
    }
}

struct SeatNode {
    seat: LockstepSeat<Game>,
    sim: Sim,
    down: Pipe,
    up: Pipe,
    /// `(capture tick, stamp)` of every submit.
    stamps: Vec<(u64, Stamp)>,
    events: Vec<SeatEvent>,
    /// `(from ms, until ms, connection kept)`: the seat neither steps, receives nor sends. With
    /// the connection kept (a stall) the stream payloads wait and arrive after it; without (the
    /// connection was lost) everything sent to the seat meanwhile is gone.
    off: Option<(u64, u64, bool)>,
    held_stream: Vec<(Lane, Vec<u8>)>,
    /// Until this ms the stream payloads to the seat wait while its datagrams arrive (a stream
    /// that lags behind the datagram lane).
    hold_stream_until: u64,
    /// `(ms, event)` of every seat event.
    timeline: Vec<(u64, SeatEvent)>,
    /// `(ms, frame tick)` of every frame stepped.
    step_times: Vec<(u64, u64)>,
    /// Frames handed out between two `outgoing` calls while catching up, at most.
    catchup_burst_max: u32,
    burst: u32,
    /// Flip the state after stepping this tick (a planted drift).
    drift_at: Option<u64>,
    /// The drift also poisons the snapshots, so a self-check replay cannot repair it.
    drift_sticky: bool,
    send_phase: u64,
    /// A modified client: an input is left out of its datagrams until the seat is this many
    /// ticks from stepping the tick it is stamped for.
    hold_inputs: Option<u64>,
}

/// What a client that sits on its inputs sends: the seat's own datagram (acknowledgement, votes)
/// without the inputs stamped past `limit`.
fn withhold(bytes: &[u8], limit: u64) -> Vec<u8> {
    let mut msg = wire::decode_inputs(bytes, 4096).expect("the seat's own datagram");
    let keep = (limit + 1).saturating_sub(msg.first_tick) as usize;
    msg.inputs.truncate(keep);
    wire::encode_inputs(&msg)
}

struct Rig {
    config: LockstepConfig,
    now: u64,
    host: LockstepHost<Game>,
    host_sim: Option<Sim>,
    host_stamps: Vec<(u64, Stamp)>,
    host_events: Vec<HostEvent>,
    host_drift_at: Option<u64>,
    host_drift_sticky: bool,
    seats: BTreeMap<SeatId, SeatNode>,
    input_of: fn(SeatId, u64) -> Pad,
    frames: Vec<LockstepFrame<Pad>>,
    /// `(ms, event)` of every host event.
    host_timeline: Vec<(u64, HostEvent)>,
    /// `(ms, frame tick)` of every frame the host built.
    build_times: Vec<(u64, u64)>,
    /// When the host last received anything from each seat.
    last_up_ms: BTreeMap<SeatId, u64>,
}

fn steady_input(seat: SeatId, capture: u64) -> Pad {
    Pad {
        move_x: (capture / 40 % 3) as i8 - 1,
        move_y: seat.0 as i8,
        aim: (capture / 7) as u32,
        buttons: u16::from(capture.is_multiple_of(50)),
        blob: Vec::new(),
    }
}

impl Rig {
    fn new(config: LockstepConfig, role: HostRole, remote: &[u16], link: LinkSpec) -> Self {
        let mut host = LockstepHost::<Game>::new(config.clone(), role);
        let mut seats = BTreeMap::new();
        if let HostRole::Listen(local) = role {
            host.add_seat(local);
        }
        for (i, id) in remote.iter().enumerate() {
            let seat = SeatId(*id);
            host.add_seat(seat);
            seats.insert(
                seat,
                SeatNode {
                    seat: LockstepSeat::new(config.clone(), seat),
                    sim: Sim::default(),
                    down: Pipe::new(link, 0x1000 + u64::from(*id)),
                    up: Pipe::new(link, 0x2000 + u64::from(*id)),
                    stamps: Vec::new(),
                    events: Vec::new(),
                    off: None,
                    held_stream: Vec::new(),
                    hold_stream_until: 0,
                    timeline: Vec::new(),
                    step_times: Vec::new(),
                    catchup_burst_max: 0,
                    burst: 0,
                    drift_at: None,
                    drift_sticky: false,
                    send_phase: (i as u64 * 5) % 16,
                    hold_inputs: None,
                },
            );
        }
        host.start(0);
        Self {
            host_sim: (role != HostRole::Relay).then(Sim::default),
            host_stamps: Vec::new(),
            host_events: Vec::new(),
            host_drift_at: None,
            host_drift_sticky: false,
            host,
            seats,
            input_of: steady_input,
            frames: Vec::new(),
            host_timeline: Vec::new(),
            build_times: Vec::new(),
            last_up_ms: BTreeMap::new(),
            now: 0,
            config,
        }
    }

    fn hash_every(&self) -> u64 {
        u64::from(self.config.hash_every)
    }

    /// One virtual millisecond.
    fn step_ms(&mut self) {
        let now = self.now;
        let hash_every = self.hash_every();
        // Seats: receive, step, sample, send.
        let mut transfers: Vec<(SeatId, u64, Voter)> = Vec::new();
        for (id, node) in &mut self.seats {
            if let Some((from, until, kept)) = node.off
                && (from..until).contains(&now)
            {
                for (lane, bytes) in node.down.due(now) {
                    if kept && lane == Lane::Stream {
                        node.held_stream.push((lane, bytes));
                    }
                }
                continue;
            }
            let due = node.down.due(now);
            let mut arrived = Vec::new();
            if now < node.hold_stream_until {
                for (lane, bytes) in due {
                    if lane == Lane::Stream {
                        node.held_stream.push((lane, bytes));
                    } else {
                        arrived.push((lane, bytes));
                    }
                }
            } else {
                arrived.extend(std::mem::take(&mut node.held_stream));
                arrived.extend(due);
            }
            for (lane, bytes) in arrived {
                node.seat.receive(lane, &bytes, now).expect("host payload");
            }
            if now == 0 {
                let stamp = node
                    .seat
                    .submit_input(&(self.input_of)(*id, 0))
                    .expect("first input");
                node.stamps.push((0, stamp));
            }
            while let Some(frame) = node.seat.next_frame() {
                if node.seat.is_catching_up() {
                    node.burst += 1;
                    node.catchup_burst_max = node.catchup_burst_max.max(node.burst);
                }
                node.step_times.push((now, frame.tick));
                node.sim.step(&frame, hash_every);
                if node.drift_at == Some(frame.tick) {
                    node.sim.state ^= 0xdead_beef;
                    node.sim.hashes.insert(frame.tick, node.sim.state);
                    if node.drift_sticky {
                        for s in node.sim.snapshots.values_mut() {
                            *s ^= 0xdead_beef;
                        }
                    }
                }
                let hash = node
                    .sim
                    .hashes
                    .get(&frame.tick)
                    .copied()
                    .unwrap_or(node.sim.state);
                node.seat.report_hash(frame.tick, hash);
                let capture = frame.tick + 1;
                match node.seat.submit_input(&(self.input_of)(*id, capture)) {
                    Ok(stamp) => node.stamps.push((capture, stamp)),
                    Err(LockstepError::MatchEnded) => {}
                    Err(e) => panic!("one input per capture: {e}"),
                }
            }
            while let Some(event) = node.seat.poll_event() {
                match event {
                    SeatEvent::SelfCheck { from, tick } => {
                        let frames: Vec<_> = node
                            .seat
                            .frames_since(from.map_or(0, |t| t + 1))
                            .cloned()
                            .collect();
                        node.sim.replay(from, frames.iter());
                        let hash = node.sim.hashes.get(&tick).copied().unwrap_or(0);
                        node.seat.revote(tick, hash);
                    }
                    SeatEvent::Resync { tick, source } => transfers.push((*id, tick, source)),
                    SeatEvent::StateReady { .. } => {
                        // The game loads the state it was sent: the rig's state is one word.
                        let (tick, bytes) = node.seat.take_state().expect("state ready");
                        let word: [u8; 8] = bytes.as_slice().try_into().expect("an 8-byte state");
                        node.sim.state = u64::from_le_bytes(word);
                        node.sim.stepped = (0..=tick).collect();
                        node.sim.hashes.insert(tick, node.sim.state);
                    }
                    _ => {}
                }
                node.timeline.push((now, event));
                node.events.push(event);
            }
            if now % 16 == node.send_phase {
                node.burst = 0;
                for mut out in node.seat.outgoing(now) {
                    if let (Some(keep), Lane::Datagram) = (node.hold_inputs, out.lane) {
                        out.bytes = withhold(&out.bytes, node.seat.tick() + keep);
                    }
                    node.up.send(now, out);
                }
            }
        }
        // The state transfer the events ask for: the rig copies the source's state at the tick and
        // the target replays its own frames after it.
        for (target, tick, source) in transfers {
            let state = match source {
                Voter::Seat(s) if self.seats.contains_key(&s) => {
                    self.seats[&s].sim.hashes.get(&tick).copied()
                }
                _ => self.host_sim.as_ref().and_then(|s| s.hashes.get(&tick).copied()),
            };
            if let (Some(state), Some(node)) = (state, self.seats.get_mut(&target)) {
                node.sim.snapshots.insert(tick, state);
                let frames: Vec<_> = node.seat.frames_since(tick + 1).cloned().collect();
                node.sim.replay(Some(tick), frames.iter());
                node.sim.hashes.insert(tick, state);
                node.seat.resynced(tick, state);
                node.drift_at = None;
            }
        }
        // Host: receive, build, step its own simulation, send.
        for (id, node) in &mut self.seats {
            for (lane, bytes) in node.up.due(now) {
                self.last_up_ms.insert(*id, now);
                match self.host.receive(*id, lane, &bytes, now) {
                    // A seat that left keeps sending for a moment; the host no longer knows it.
                    Ok(()) | Err(LockstepError::UnknownSeat(_)) => {}
                    Err(e) => panic!("seat payload: {e}"),
                }
            }
        }
        if now == 0
            && let HostRole::Listen(local) = self.host.role()
        {
            let stamp = self
                .host
                .local_input(&(self.input_of)(local, 0), now)
                .expect("host input");
            self.host_stamps.push((0, stamp));
        }
        while let Some(frame) = self.host.poll_frame(now) {
            self.build_times.push((now, frame.tick));
            if let Some(sim) = &mut self.host_sim {
                sim.step(&frame, hash_every);
                if self.host_drift_at == Some(frame.tick) {
                    sim.state ^= 0xdead_beef;
                    sim.hashes.insert(frame.tick, sim.state);
                    if self.host_drift_sticky {
                        for s in sim.snapshots.values_mut() {
                            *s ^= 0xdead_beef;
                        }
                    }
                }
                let hash = sim.hashes.get(&frame.tick).copied().unwrap_or(sim.state);
                self.host.report_hash(frame.tick, hash);
            }
            if let HostRole::Listen(local) = self.host.role()
                && !frame.match_end
            {
                let capture = frame.tick + 1;
                let stamp = self
                    .host
                    .local_input(&(self.input_of)(local, capture), now)
                    .expect("host input");
                self.host_stamps.push((capture, stamp));
            }
            self.frames.push(frame);
        }
        while let Some(event) = self.host.poll_event() {
            match &event {
                HostEvent::SelfCheck { from, tick } => {
                    if let Some(sim) = &mut self.host_sim {
                        let frames: Vec<_> = self
                            .host
                            .frames_since(from.map_or(0, |t| t + 1))
                            .cloned()
                            .collect();
                        sim.replay(*from, frames.iter());
                        let hash = sim.hashes.get(tick).copied().unwrap_or(0);
                        self.host.revote(*tick, hash);
                    }
                }
                HostEvent::ResyncHost { source, tick, .. } => {
                    let state = match source {
                        Voter::Seat(s) => self.seats.get(s).and_then(|n| n.sim.hashes.get(tick).copied()),
                        Voter::Observer => None,
                    };
                    if let (Some(state), Some(sim)) = (state, &mut self.host_sim) {
                        sim.snapshots.insert(*tick, state);
                        let frames: Vec<_> = self.host.frames_since(tick + 1).cloned().collect();
                        sim.replay(Some(*tick), frames.iter());
                        sim.hashes.insert(*tick, state);
                        self.host.revote(*tick, state);
                        self.host_drift_at = None;
                    }
                }
                _ => {}
            }
            self.host_timeline.push((now, event.clone()));
            self.host_events.push(event);
        }
        if now.is_multiple_of(16) {
            for (seat, out) in self.host.outgoing(now) {
                if out.lane == Lane::Datagram {
                    assert!(
                        out.bytes.len() <= self.config.datagram_budget_bytes,
                        "a {}-byte datagram over the {}-byte budget",
                        out.bytes.len(),
                        self.config.datagram_budget_bytes
                    );
                }
                if let Some(node) = self.seats.get_mut(&seat) {
                    node.down.send(now, out);
                }
            }
        }
        self.now += 1;
    }

    fn run_ms(&mut self, ms: u64) {
        for _ in 0..ms {
            self.step_ms();
        }
    }

    fn set_links(&mut self, link: LinkSpec) {
        for node in self.seats.values_mut() {
            node.down.spec = link;
            node.up.spec = link;
        }
    }

    /// Every seat (and the host's simulation) stepped the ticks `0..n` in order, each once, and
    /// they agree on the state after the last tick they all stepped.
    fn assert_in_step(&self) {
        let mut common = u64::MAX;
        for (id, node) in &self.seats {
            assert!(
                node.sim
                    .stepped
                    .iter()
                    .copied()
                    .eq(0..node.sim.stepped.len() as u64),
                "{id} stepped out of order or twice"
            );
            common = common.min(node.sim.stepped.len() as u64);
        }
        if let Some(sim) = &self.host_sim {
            assert!(sim.stepped.iter().copied().eq(0..sim.stepped.len() as u64));
            common = common.min(sim.stepped.len() as u64);
        }
        assert!(common > 0 && common != u64::MAX, "nothing stepped");
        let tick = common - 1;
        let mut hashes = BTreeSet::new();
        for node in self.seats.values() {
            hashes.insert(node.sim.hashes[&tick]);
        }
        if let Some(sim) = &self.host_sim {
            hashes.insert(sim.hashes[&tick]);
        }
        assert_eq!(hashes.len(), 1, "state differs at tick {tick}");
    }

    fn desyncs(&self) -> Vec<&DesyncVerdict> {
        self.host_events
            .iter()
            .filter_map(|e| match e {
                HostEvent::Desync(v) => Some(v),
                _ => None,
            })
            .collect()
    }
}

fn config() -> LockstepConfig {
    LockstepConfig::default()
}

// ---- frames, order and inputs ---------------------------------------------------------------

#[test]
fn every_seat_steps_the_same_frames_in_order_once() {
    let mut rig = Rig::new(
        config(),
        HostRole::Listen(SeatId(0)),
        &[1, 2, 3],
        LinkSpec::latency(10),
    );
    rig.run_ms(5_000);
    rig.assert_in_step();
    assert!(
        rig.frames.len() >= 299,
        "{} frames in 5 s at 60 Hz",
        rig.frames.len()
    );
    assert!(rig.desyncs().is_empty());
    let status = rig.host.status(rig.now);
    assert!(status.checkpoints_agreed >= 8, "{status:?}");
    assert_eq!(status.seats.len(), 4);
    // Frames list the four seats in ascending order on every tick.
    for frame in &rig.frames {
        let seats: Vec<u16> = frame.inputs.iter().map(|(s, _)| s.0).collect();
        assert_eq!(seats, [0, 1, 2, 3], "tick {}", frame.tick);
    }
}

#[test]
fn an_input_sampled_at_a_capture_tick_is_in_the_frame_delay_ticks_later() {
    let mut rig = Rig::new(config(), HostRole::Relay, &[1, 2], LinkSpec::CLEAN);
    rig.run_ms(3_000);
    let delay = u64::from(rig.config.initial_delay_ticks);
    let mut checked = 0;
    for frame in rig.frames.iter().filter(|f| f.tick >= delay + 2) {
        for (seat, input) in &frame.inputs {
            // Clean 1 ms links and a 4-tick delay: every input is on time.
            assert_eq!(
                input,
                &SeatInput::Present(steady_input(*seat, frame.tick - delay)),
                "tick {} {seat}",
                frame.tick
            );
            checked += 1;
        }
    }
    assert!(checked > 300);
    // The start is a raise from zero: the first `delay` ticks are gap ticks, empty for every seat.
    for frame in rig.frames.iter().take(delay as usize) {
        assert!(frame.inputs.iter().all(|(_, i)| *i == SeatInput::Absent));
    }
    assert_eq!(rig.host.status(rig.now).gap_ticks, delay);
}

// ---- redundancy -----------------------------------------------------------------------------

#[test]
fn one_lost_frame_datagram_is_repaired_by_the_next_within_two_ticks() {
    let mut rig = Rig::new(config(), HostRole::Relay, &[1, 2, 3, 4], LinkSpec::latency(5));
    rig.run_ms(1_000);
    let before = rig.seats[&SeatId(2)].sim.stepped.len();
    // Lose exactly one frames datagram toward seat 2.
    rig.seats.get_mut(&SeatId(2)).expect("seat 2").down.drop_next = 1;
    let mut worst_gap_ms = 0u64;
    let mut last_step_at = rig.now;
    let mut stepped = before;
    for _ in 0..1_000 {
        rig.step_ms();
        let now_stepped = rig.seats[&SeatId(2)].sim.stepped.len();
        if now_stepped > stepped {
            worst_gap_ms = worst_gap_ms.max(rig.now - last_step_at);
            last_step_at = rig.now;
            stepped = now_stepped;
        }
    }
    rig.assert_in_step();
    let status = rig.seats[&SeatId(2)].seat.status();
    assert_eq!(status.datagrams_lost, 1);
    assert!(
        status.frames_repaired >= 1,
        "the lost frame came with the next datagram"
    );
    assert_eq!(status.repair_ticks_max, 1, "one tick late, not a resend timer");
    assert_eq!(
        (status.single_losses, status.single_loss_repair_ticks_max),
        (1, 1)
    );
    // No stall over two ticks: the longest wait between two stepped frames stays under 3 ticks.
    assert!(worst_gap_ms <= 2 * 17 + 1, "stalled {worst_gap_ms} ms");
    // The other seats never noticed.
    for id in [1, 3, 4] {
        assert_eq!(rig.seats[&SeatId(id)].seat.status().frames_repaired, 0);
    }
    assert_eq!(
        rig.seats[&SeatId(2)].seat.stream_stats().0,
        0,
        "no stream fallback"
    );
}

#[test]
fn with_every_unacked_frame_a_burst_is_repaired_by_the_first_datagram_that_arrives() {
    let config = LockstepConfig {
        redundancy: FrameRedundancy::AllUnacked,
        ..config()
    };
    let mut rig = Rig::new(config, HostRole::Relay, &[1, 2], LinkSpec::latency(5));
    rig.run_ms(500);
    rig.seats.get_mut(&SeatId(1)).expect("seat 1").down.drop_next = 6;
    rig.run_ms(1_500);
    rig.assert_in_step();
    let status = rig.seats[&SeatId(1)].seat.status();
    assert_eq!(status.datagrams_lost, 6);
    assert_eq!(
        status.repair_ticks_max, 6,
        "six frames came with the seventh datagram"
    );
}

#[test]
fn a_burst_longer_than_the_redundancy_window_is_reported_as_a_hole_and_repaired() {
    // Clean 30 ms links: the window is two frames. Five datagrams in a row are lost.
    let mut rig = Rig::new(config(), HostRole::Relay, &[1, 2], LinkSpec::latency(30));
    rig.run_ms(1_000);
    rig.seats.get_mut(&SeatId(1)).expect("seat 1").down.drop_next = 5;
    rig.run_ms(2_000);
    rig.assert_in_step();
    let status = rig.host.status(rig.now);
    assert_eq!(
        status.seats[0].hole_repairs, 1,
        "the seat said frames were missing, once"
    );
    assert_eq!(status.seats[1].hole_repairs, 0);
    let seat = &rig.seats[&SeatId(1)];
    assert_eq!(
        seat.seat.stream_stats().0,
        0,
        "repaired by datagrams, not the stream"
    );
    assert!(seat.sim.stepped.len() as u64 + 10 > rig.host.tick(), "caught up");
    // The stall is the burst plus the round trip of the report: 5 + about 4 + 2 ticks.
    let repair = seat.seat.status().repair_ticks_max;
    assert!((5..=12).contains(&repair), "{repair} ticks");
    // The seat now knows its link loses five in a row: the window covers the next such burst.
    rig.seats.get_mut(&SeatId(1)).expect("seat 1").down.drop_next = 5;
    rig.run_ms(2_000);
    rig.assert_in_step();
    assert_eq!(
        rig.host.status(rig.now).seats[0].hole_repairs,
        1,
        "no second hole"
    );
    assert_eq!(
        rig.seats[&SeatId(1)].seat.status().repair_ticks_max,
        repair.max(5)
    );
}

#[test]
fn the_redundancy_window_costs_less_than_every_unacked_frame_on_a_clean_slow_link() {
    let run = |redundancy: FrameRedundancy| {
        let config = LockstepConfig {
            redundancy,
            ..config()
        };
        let mut rig = Rig::new(config, HostRole::Relay, &[1, 2, 3, 4], LinkSpec::latency(50));
        rig.run_ms(10_000);
        rig.assert_in_step();
        let status = rig.host.status(rig.now);
        (status.datagram_bytes_sent, status.datagrams_sent)
    };
    let (window, window_count) = run(FrameRedundancy::Adaptive);
    let (all, all_count) = run(FrameRedundancy::AllUnacked);
    assert_eq!(
        window_count, all_count,
        "one frames datagram per seat per tick either way"
    );
    // 100 ms round trips: about eight frames unacknowledged, two in the window. With four seats a
    // frame is a few bytes and the datagram header is the rest, so the bytes fall by about half.
    assert!(
        window * 10 < all * 6,
        "{window} bytes with the window, {all} with every unacked frame"
    );
}

#[test]
fn bad_wifi_for_a_minute_keeps_every_seat_in_step_without_the_stream() {
    let mut rig = Rig::new(
        config(),
        HostRole::Listen(SeatId(0)),
        &[1, 2, 3],
        LinkSpec::BAD_WIFI,
    );
    rig.run_ms(60_000);
    rig.assert_in_step();
    assert!(rig.desyncs().is_empty(), "{:?}", rig.desyncs());
    for node in rig.seats.values() {
        let s = node.seat.status();
        assert!(s.datagrams_lost > 20, "the link really lost datagrams: {s:?}");
        assert!(s.frames_repaired > 20);
        assert_eq!(
            node.seat.stream_stats().0,
            0,
            "redundancy alone repaired every loss"
        );
        // Duplicates and reordering never made a seat step out of order (assert_in_step) and
        // every seat kept up with the host.
        assert!(node.sim.stepped.len() as u64 + 30 > rig.host.tick());
    }
}

// ---- delay control --------------------------------------------------------------------------

fn delay_changes(events: &[SeatEvent]) -> Vec<(u16, u64)> {
    events
        .iter()
        .filter_map(|e| match e {
            SeatEvent::DelayChanged { delay, delay_from } => Some((*delay, *delay_from)),
            _ => None,
        })
        .collect()
}

#[test]
fn a_delay_raise_and_a_lower_switch_on_the_same_tick_on_every_seat_and_no_tick_twice() {
    let mut rig = Rig::new(
        config(),
        HostRole::Listen(SeatId(0)),
        &[1, 2, 3],
        LinkSpec::latency(5),
    );
    rig.run_ms(3_000);
    // The links get slow: the delay must rise at once, well before the two-second window ends.
    rig.set_links(LinkSpec::latency(60));
    rig.run_ms(1_000);
    let raised = rig.host.delay_ticks();
    assert!(
        raised >= 8,
        "120 ms round trips need at least 8 ticks, got {raised}"
    );
    assert!(
        rig.host_events
            .iter()
            .any(|e| matches!(e, HostEvent::DelayChanged { raised: true, .. }))
    );
    // Then fast again: one tick lower after five calm windows, never sooner.
    rig.set_links(LinkSpec::latency(5));
    rig.run_ms(9_000);
    assert_eq!(
        rig.host.delay_ticks(),
        raised,
        "five calm windows have not passed"
    );
    rig.run_ms(26_000);
    let lowered = rig.host.delay_ticks();
    assert!(lowered < raised, "the delay came down: {raised} -> {lowered}");
    rig.assert_in_step();
    assert!(rig.desyncs().is_empty());

    // Every seat saw the same epochs, in the same order.
    let reference = delay_changes(&rig.seats[&SeatId(1)].events);
    assert!(
        reference.len() >= 3,
        "a raise and at least two lowers: {reference:?}"
    );
    for node in rig.seats.values() {
        assert_eq!(delay_changes(&node.events), reference);
    }
    let host_epochs: Vec<(u16, u64)> = rig
        .host_events
        .iter()
        .filter_map(|e| match e {
            HostEvent::DelayChanged {
                delay, delay_from, ..
            } => Some((*delay, *delay_from)),
            _ => None,
        })
        .collect();
    assert_eq!(host_epochs, reference);
    // Lowers are one tick each.
    for pair in reference.windows(2) {
        if pair[1].0 < pair[0].0 {
            assert_eq!(pair[1].0 + 1, pair[0].0, "{reference:?}");
        }
    }

    // The same capture tick gets the same stamp on every seat and on the host, and no tick is
    // stamped twice.
    let reference_stamps: BTreeMap<u64, Stamp> = rig.seats[&SeatId(1)].stamps.iter().copied().collect();
    for node in rig.seats.values() {
        let mut seen = BTreeSet::new();
        for (capture, stamp) in &node.stamps {
            if let Some(expected) = reference_stamps.get(capture) {
                assert_eq!(stamp, expected, "capture {capture}");
            }
            if let Stamp::Tick(t) = stamp {
                assert!(seen.insert(*t), "tick {t} stamped twice");
            }
        }
    }
    for (capture, stamp) in &rig.host_stamps {
        if let Some(expected) = reference_stamps.get(capture) {
            assert_eq!(stamp, expected, "the host's own input, capture {capture}");
        }
    }
    let skipped = reference_stamps
        .values()
        .filter(|s| **s == Stamp::Skipped)
        .count();
    assert!(skipped >= 2, "each lower skips one capture");

    // Gap ticks of the raise are empty for every seat, and only those ticks are.
    let mut gap_ticks = 0u64;
    for frame in &rig.frames {
        if frame.inputs.iter().all(|(_, i)| *i == SeatInput::Absent) {
            gap_ticks += 1;
        }
    }
    let status = rig.host.status(rig.now);
    assert!(
        gap_ticks >= status.gap_ticks,
        "{gap_ticks} empty frames, {} gap ticks",
        status.gap_ticks
    );
    assert!(
        status.gap_ticks >= u64::from(raised),
        "the start gap plus the raise gap"
    );
    assert!(status.delay_raises >= 1 && status.delay_lowers >= 2);
}

#[test]
fn the_delay_is_sized_from_measured_lateness_and_loss_runs_not_round_trip_time() {
    // Two rigs with the same 10 ms latency (the same round-trip time): one clean, one that also
    // loses datagrams in bursts. Only the measured loss runs can tell them apart.
    let mut clean = Rig::new(config(), HostRole::Relay, &[1, 2], LinkSpec::latency(10));
    let mut bursty = Rig::new(
        config(),
        HostRole::Relay,
        &[1, 2],
        LinkSpec {
            burst_start_per_mille: 20,
            burst_stay_per_mille: 500,
            ..LinkSpec::latency(10)
        },
    );
    clean.run_ms(40_000);
    bursty.run_ms(40_000);
    let c = clean.host.status(clean.now);
    let b = bursty.host.status(bursty.now);
    let clean_run = c.seats.iter().map(|s| s.loss_run_p99).max().unwrap_or(0);
    let bursty_run = b.seats.iter().map(|s| s.loss_run_p99).max().unwrap_or(0);
    assert_eq!(clean_run, 0);
    assert!(
        bursty_run >= 3,
        "bursts that continue half the time: p99 run {bursty_run}"
    );
    assert!(
        b.delay_ticks >= c.delay_ticks + 3,
        "same latency, more delay for the bursty link: {} vs {}",
        b.delay_ticks,
        c.delay_ticks
    );
    assert!(b.delay_ticks <= bursty.config.max_delay_ticks);
    clean.assert_in_step();
    bursty.assert_in_step();
}

#[test]
fn the_delay_never_passes_the_maximum() {
    let mut rig = Rig::new(config(), HostRole::Relay, &[1, 2], LinkSpec::latency(400));
    rig.run_ms(10_000);
    assert_eq!(rig.host.delay_ticks(), 15);
    assert_eq!(LockstepConfig::at_rate(30).max_delay_ticks, 8, "250 ms at 30 Hz");
    assert_eq!(LockstepConfig::at_rate(120).max_delay_ticks, 30);
}

#[test]
fn per_seat_delay_follows_the_median_seat_and_the_slow_seat_stamps_further_ahead() {
    let run = |policy: DelayPolicy| {
        let config = LockstepConfig {
            delay_policy: policy,
            ..config()
        };
        let mut rig = Rig::new(config, HostRole::Relay, &[1, 2, 3], LinkSpec::latency(10));
        // Seat 3 is on a much slower link than the other two.
        let slow = rig.seats.get_mut(&SeatId(3)).expect("seat 3");
        slow.down.spec = LinkSpec::latency(70);
        slow.up.spec = LinkSpec::latency(70);
        rig.run_ms(10_000);
        let missing_then = rig.host.status(rig.now).seats[2].inputs_missing;
        rig.run_ms(10_000);
        rig.assert_in_step();
        let status = rig.host.status(rig.now);
        let missing_late = status.seats[2].inputs_missing - missing_then;
        (status, missing_late)
    };
    let (per_seat, slow_missing) = run(DelayPolicy::PerSeat);
    let (shared, _) = run(DelayPolicy::Shared);
    assert!(
        shared.delay_ticks >= 9,
        "shared: everyone pays for the 140 ms round trip: {}",
        shared.delay_ticks
    );
    assert!(
        per_seat.delay_ticks <= 5,
        "per seat: the frame delay follows the median seat: {}",
        per_seat.delay_ticks
    );
    assert_eq!(per_seat.seats[0].extra_lead_ticks, 0);
    assert_eq!(per_seat.seats[1].extra_lead_ticks, 0);
    assert!(
        per_seat.seats[2].extra_lead_ticks >= 4,
        "the slow seat stamps further ahead on its own: {}",
        per_seat.seats[2].extra_lead_ticks
    );
    assert!(
        per_seat.seats[2].extra_lead_ticks + per_seat.delay_ticks >= 9,
        "its total lead covers its round trip"
    );
    // Once its lead settled its inputs are in the frames: under 2 % of 600 ticks missing.
    assert!(
        slow_missing <= 12,
        "{slow_missing} of 600 ticks without the slow seat's input"
    );
}

#[test]
fn a_late_grace_holds_a_due_frame_for_a_live_seats_input() {
    for (grace, held) in [(0u64, false), (10, true)] {
        let config = LockstepConfig {
            late_grace_ms: grace,
            ..config()
        };
        let mut host = LockstepHost::<Game>::new(config.clone(), HostRole::Relay);
        let mut seat = LockstepSeat::<Game>::new(config, SeatId(1));
        host.add_seat(SeatId(1));
        host.start(0);
        let pad = Pad {
            buttons: 7,
            ..Pad::default()
        };
        assert_eq!(seat.submit_input(&pad), Ok(Stamp::Tick(4)));
        let out = seat.outgoing(0);
        // An empty datagram makes the seat live; its input for tick 4 arrives at 71 ms, 4 ms after
        // tick 4 is due (66.7 ms).
        host.receive(
            SeatId(1),
            Lane::Datagram,
            &wire::encode_inputs(&wire::InputsMsg::default()),
            0,
        )
        .expect("empty inputs");
        let mut built = Vec::new();
        for now in 0..=100u64 {
            if now == 71 {
                host.receive(SeatId(1), Lane::Datagram, &out[0].bytes, now)
                    .expect("the input");
            }
            while let Some(frame) = host.poll_frame(now) {
                built.push((now, frame));
            }
        }
        let (at, frame) = built.iter().find(|(_, f)| f.tick == 4).expect("frame 4");
        if held {
            assert_eq!(*at, 71, "held until the input arrived");
            assert_eq!(frame.inputs[0].1, SeatInput::Present(pad.clone()));
        } else {
            assert_eq!(*at, 67, "built at its time");
            assert_eq!(frame.inputs[0].1, SeatInput::Absent);
        }
        // The next frame is due at 83.3 ms whatever happened to this one. The seat sent nothing for
        // it: without a grace it is built at its time, with one it is held for the whole grace.
        let next = built.iter().find(|(_, f)| f.tick == 5).map(|(at, _)| *at);
        assert_eq!(next, Some(if held { 94 } else { 84 }));
    }
}

// ---- the player host's input ----------------------------------------------------------------

fn host_input_ticks(mode: HostInput) -> (Vec<(u64, Stamp)>, Vec<LockstepFrame<Pad>>, u64) {
    let config = LockstepConfig {
        host_input: mode,
        ..config()
    };
    let delay = u64::from(config.initial_delay_ticks);
    let mut rig = Rig::new(config, HostRole::Listen(SeatId(0)), &[1], LinkSpec::CLEAN);
    rig.run_ms(2_000);
    rig.assert_in_step();
    (rig.host_stamps.clone(), rig.frames.clone(), delay)
}

#[test]
fn the_player_hosts_input_is_delayed_like_a_seats_when_delayed() {
    let (stamps, frames, delay) = host_input_ticks(HostInput::Delayed);
    for (capture, stamp) in &stamps {
        assert_eq!(*stamp, Stamp::Tick(capture + delay), "capture {capture}");
    }
    // The frame at tick T carries the host's input sampled at capture T - delay, exactly like
    // the remote seat's: no host advantage.
    for frame in frames.iter().filter(|f| f.tick >= delay + 2) {
        let capture = frame.tick - delay;
        assert_eq!(
            frame.inputs[0],
            (SeatId(0), SeatInput::Present(steady_input(SeatId(0), capture)))
        );
        assert_eq!(
            frame.inputs[1],
            (SeatId(1), SeatInput::Present(steady_input(SeatId(1), capture)))
        );
    }
}

#[test]
fn the_player_hosts_input_goes_into_the_next_frame_when_immediate() {
    let (stamps, frames, delay) = host_input_ticks(HostInput::Immediate);
    for (capture, stamp) in &stamps {
        // The first `delay` ticks are gap ticks: their frames carry no input for anyone, and the
        // host is told so instead of being handed a tick that drops its input.
        let want = if *capture < delay {
            Stamp::Skipped
        } else {
            Stamp::Tick(*capture)
        };
        assert_eq!(*stamp, want, "capture {capture}");
    }
    for frame in frames.iter().filter(|f| f.tick < delay) {
        assert!(
            frame.inputs.iter().all(|(_, i)| *i == SeatInput::Absent),
            "gap tick {}",
            frame.tick
        );
    }
    for frame in frames.iter().filter(|f| f.tick >= delay) {
        assert_eq!(
            frame.inputs[0],
            (SeatId(0), SeatInput::Present(steady_input(SeatId(0), frame.tick))),
            "the host's input is in the frame it was stamped for: tick {}",
            frame.tick
        );
    }
    for frame in frames.iter().filter(|f| f.tick >= delay + 2) {
        assert_eq!(
            frame.inputs[0],
            (SeatId(0), SeatInput::Present(steady_input(SeatId(0), frame.tick))),
            "the host's input is {delay} ticks fresher than the seat's"
        );
        assert_eq!(
            frame.inputs[1],
            (
                SeatId(1),
                SeatInput::Present(steady_input(SeatId(1), frame.tick - delay))
            )
        );
    }
}

#[test]
fn a_second_input_for_one_capture_tick_is_refused() {
    let mut seat = LockstepSeat::<Game>::new(config(), SeatId(3));
    assert_eq!(seat.submit_input(&Pad::default()), Ok(Stamp::Tick(4)));
    assert_eq!(
        seat.submit_input(&Pad::default()),
        Err(LockstepError::CaptureClosed)
    );
    let mut host = LockstepHost::<Game>::new(config(), HostRole::Listen(SeatId(0)));
    host.add_seat(SeatId(0));
    assert_eq!(
        host.local_input(&Pad::default(), 0),
        Err(LockstepError::NotStarted)
    );
    host.start(0);
    assert_eq!(host.local_input(&Pad::default(), 0), Ok(Stamp::Tick(4)));
    assert_eq!(
        host.local_input(&Pad::default(), 0),
        Err(LockstepError::CaptureClosed)
    );
    let mut relay = LockstepHost::<Game>::new(config(), HostRole::Relay);
    relay.start(0);
    assert_eq!(
        relay.local_input(&Pad::default(), 0),
        Err(LockstepError::NoLocalSeat)
    );
    assert_eq!(LockstepError::NoLocalSeat.code(), "lockstep_no_local_seat");
}

// ---- wire: delta coding, the budget and the stream ------------------------------------------

fn raw(tick: u64, entries: Vec<(u16, Option<Vec<u8>>)>) -> RawFrame {
    RawFrame {
        tick,
        delay_change: None,
        membership: Vec::new(),
        gap: false,
        entries: entries.into_iter().map(|(s, e)| (SeatId(s), e)).collect(),
        match_end: false,
    }
}

fn joined(seats: u16) -> (FrameState, FrameState, Vec<u8>) {
    let mut enc = FrameState::new(4);
    let mut dec = FrameState::new(4);
    let join = RawFrame {
        tick: 0,
        delay_change: None,
        membership: (0..seats)
            .map(|s| MembershipChange {
                seat: SeatId(s),
                kind: MembershipKind::Join,
            })
            .collect(),
        gap: true,
        entries: Vec::new(),
        match_end: false,
    };
    let body = wire::encode_frame_body(&join, &mut enc);
    let back = wire::decode_frame_body(&body, 0, &mut dec, 1024).expect("join frame");
    assert_eq!(back, join);
    (enc, dec, body)
}

#[test]
fn an_unchanged_seat_costs_one_bit_and_a_changed_byte_about_three_bytes() {
    let (mut enc, mut dec, _) = joined(40);
    let input = |seat: u16, aim: u8| -> Vec<u8> {
        let mut b = vec![0u8; 12];
        b[0] = seat as u8;
        b[5] = aim;
        b
    };
    let first = raw(1, (0..40).map(|s| (s, Some(input(s, 0)))).collect());
    let body = wire::encode_frame_body(&first, &mut enc);
    assert_eq!(
        wire::decode_frame_body(&body, 1, &mut dec, 1024).expect("first"),
        first
    );
    assert!(
        body.len() > 40 * 12,
        "the first frame carries every input in full"
    );

    // Nothing changed: 3 flag bits + 40 bits.
    let same = raw(2, (0..40).map(|s| (s, Some(input(s, 0)))).collect());
    let body = wire::encode_frame_body(&same, &mut enc);
    assert_eq!(body.len(), 6, "43 bits");
    assert_eq!(
        wire::decode_frame_body(&body, 2, &mut dec, 1024).expect("same"),
        same
    );

    // Three seats moved their aim by one byte: 3 x (3 + 12 + 8) bits more.
    let moved = raw(
        3,
        (0..40)
            .map(|s| (s, Some(input(s, u8::from(s % 16 == 1)))))
            .collect(),
    );
    let body = wire::encode_frame_body(&moved, &mut enc);
    assert!(
        body.len() <= 6 + 9,
        "{} bytes for three changed seats",
        body.len()
    );
    assert_eq!(
        wire::decode_frame_body(&body, 3, &mut dec, 1024).expect("moved"),
        moved
    );

    // A seat goes absent and comes back with the input it had: two bits, then a zero mask.
    let mut entries: Vec<(u16, Option<Vec<u8>>)> = (0..40)
        .map(|s| (s, Some(input(s, u8::from(s % 16 == 1)))))
        .collect();
    entries[7].1 = None;
    let absent = raw(4, entries.clone());
    let body = wire::encode_frame_body(&absent, &mut enc);
    assert_eq!(
        wire::decode_frame_body(&body, 4, &mut dec, 1024).expect("absent"),
        absent
    );
    entries[7].1 = Some(input(7, 0));
    let back = raw(5, entries);
    let body = wire::encode_frame_body(&back, &mut enc);
    assert!(body.len() <= 6 + 2);
    assert_eq!(
        wire::decode_frame_body(&body, 5, &mut dec, 1024).expect("back"),
        back
    );
    assert_eq!(enc, dec, "both ends hold the same delta base");
}

#[test]
fn delta_coding_compares_bytes_so_a_negative_zero_is_a_change() {
    // +0.0 == -0.0 and NaN != NaN as floats. The frame compares the bytes the seat sent, so every
    // participant gets exactly the bits the host did, whatever the game's codec writes.
    let (mut enc, mut dec, _) = joined(1);
    let plus = 0.0f32.to_le_bytes().to_vec();
    let minus = (-0.0f32).to_le_bytes().to_vec();
    let nan = f32::from_bits(0x7fc0_0001).to_le_bytes().to_vec();
    for (tick, bytes) in [(1u64, &plus), (2, &minus), (3, &nan), (4, &nan), (5, &plus)] {
        let frame = raw(tick, vec![(0, Some(bytes.clone()))]);
        let body = wire::encode_frame_body(&frame, &mut enc);
        let back = wire::decode_frame_body(&body, tick, &mut dec, 1024).expect("frame");
        assert_eq!(back.entries[0].1.as_ref(), Some(bytes), "tick {tick}: exact bits");
    }
}

#[test]
fn frames_that_do_not_fit_a_datagram_go_on_the_stream_and_every_seat_still_steps_them() {
    let config = LockstepConfig {
        datagram_budget_bytes: 400,
        ..config()
    };
    let mut rig = Rig::new(config, HostRole::Relay, &[1, 2, 3], LinkSpec::latency(20));
    // Every seat sends a new 200-byte blob every tick: one frame is about 620 bytes.
    rig.input_of = |seat, capture| Pad {
        blob: (0..200u32)
            .map(|i| (i as u64 + capture + u64::from(seat.0)) as u8)
            .collect(),
        ..steady_input(seat, capture)
    };
    rig.run_ms(3_000);
    rig.assert_in_step();
    for node in rig.seats.values() {
        let (stream_frames, _) = node.seat.stream_stats();
        assert!(
            stream_frames > 100,
            "oversize frames arrived on the stream: {stream_frames}"
        );
    }
    let status = rig.host.status(rig.now);
    assert!(status.stream_bytes_sent > 100_000);
    assert!(status.seats.iter().all(|s| s.stream_frames > 100 && !s.lagging));
    // Present inputs still carry the blob, bit for bit.
    let last = rig.frames.last().expect("frames");
    assert!(
        last.inputs
            .iter()
            .all(|(_, i)| matches!(i, SeatInput::Present(p) if p.blob.len() == 200))
    );
}

#[test]
fn an_input_over_the_limit_or_malformed_is_refused_and_leaves_no_trace() {
    let config = LockstepConfig {
        max_input_bytes: 64,
        ..config()
    };
    let mut host = LockstepHost::<Game>::new(config.clone(), HostRole::Relay);
    host.add_seat(SeatId(1));
    host.start(0);
    let mut seat = LockstepSeat::<Game>::new(
        LockstepConfig {
            max_input_bytes: 4096,
            ..config.clone()
        },
        SeatId(1),
    );
    let big = Pad {
        blob: vec![7; 300],
        ..Pad::default()
    };
    seat.submit_input(&big).expect("the seat's own limit allows it");
    let out = seat.outgoing(0);
    let err = host
        .receive(SeatId(1), Lane::Datagram, &out[0].bytes, 0)
        .expect_err("over the host's limit");
    assert_eq!(err.code(), "lockstep_input_too_large");
    // A seat that is not part of the match.
    assert_eq!(
        host.receive(SeatId(9), Lane::Datagram, &out[0].bytes, 0),
        Err(LockstepError::UnknownSeat(SeatId(9)))
    );
    // Bytes that are not the game's input: three bytes cannot be a `Pad`.
    let msg = wire::InputsMsg {
        first_tick: 4,
        inputs: vec![Some(vec![1, 2, 3])],
        ..wire::InputsMsg::default()
    };
    let err = host
        .receive(SeatId(1), Lane::Datagram, &wire::encode_inputs(&msg), 0)
        .expect_err("not a Pad");
    assert_eq!(err.code(), "lockstep_input_invalid");
    // The wrong lane, and a seat's own limit.
    assert!(host.receive(SeatId(1), Lane::Stream, &out[0].bytes, 0).is_err());
    let mut strict = LockstepSeat::<Game>::new(config, SeatId(1));
    assert_eq!(
        strict.submit_input(&big).map_err(|e| e.code()),
        Err("lockstep_input_too_large")
    );
    // Nothing of it reached a frame.
    let frame = (0..200).find_map(|ms| host.poll_frame(ms)).expect("frame 0");
    assert!(frame.inputs.iter().all(|(_, i)| *i == SeatInput::Absent));
}

#[test]
fn hostile_bytes_never_panic_a_decoder() {
    let mut rng = Rng(0xfeed);
    let config = config();
    let mut host = LockstepHost::<Game>::new(config.clone(), HostRole::Relay);
    host.add_seat(SeatId(1));
    host.start(0);
    let mut seat = LockstepSeat::<Game>::new(config, SeatId(1));
    for round in 0..20_000u64 {
        let len = rng.below(96) as usize;
        let mut bytes: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
        // Half the time start with a real kind byte so the decoders past the dispatch run.
        if round.is_multiple_of(2) && !bytes.is_empty() {
            bytes[0] = [0x01, 0x02, 0x10, 0x11, 0x20, 0x21][rng.below(6) as usize];
        }
        for lane in [Lane::Datagram, Lane::Stream] {
            let _ = host.receive(SeatId(1), lane, &bytes, round);
            let _ = seat.receive(lane, &bytes, round);
        }
        let mut state = FrameState::new(4);
        state.seats.entry(SeatId(1)).or_default();
        let _ = wire::decode_frame_body(&bytes, round, &mut state, 1024);
    }
    // A length that claims more than the payload holds allocates nothing and errors.
    let claim = [0x10, 0x00, 0xff, 0x01];
    assert!(wire::decode_stream_frames(&claim).is_err());
    // The host still builds and the seat still steps.
    assert!((0..100).any(|ms| host.poll_frame(30_000 + ms).is_some()));
}

#[test]
fn a_seat_that_was_cut_off_catches_up_and_one_past_the_frame_log_is_lagging() {
    let config = LockstepConfig {
        frame_log_ticks: 120,
        ..config()
    };
    let mut rig = Rig::new(config, HostRole::Relay, &[1, 2], LinkSpec::latency(5));
    rig.run_ms(1_000);
    // One second of nothing toward seat 1 (60 frames datagrams): inside the two-second log.
    rig.seats.get_mut(&SeatId(1)).expect("seat 1").down.drop_next = 60;
    rig.run_ms(3_000);
    rig.assert_in_step();
    assert!(!rig.host.status(rig.now).seats[0].lagging);
    // Four seconds of nothing in both directions: the frames it needs are gone from the log.
    let node = rig.seats.get_mut(&SeatId(1)).expect("seat 1");
    node.down.drop_next = 240;
    node.up.drop_next = 240;
    rig.run_ms(5_000);
    assert!(
        rig.host_events
            .contains(&HostEvent::SeatLagging { seat: SeatId(1) })
    );
    assert!(rig.host.status(rig.now).seats[0].lagging);
    // The other seat never stalled.
    assert!(rig.seats[&SeatId(2)].sim.stepped.len() as u64 + 10 > rig.host.tick());
}

#[test]
fn a_seat_that_stops_stepping_takes_no_more_frames_than_its_log_holds() {
    // Two budgets: frames that fit a datagram, and frames that overflow to the stream at once.
    for budget in [1_000usize, 128] {
        let config = LockstepConfig {
            frame_log_ticks: 64,
            datagram_budget_bytes: budget,
            ..config()
        };
        let mut host = LockstepHost::<Game>::new(config.clone(), HostRole::Relay);
        let mut seat = LockstepSeat::<Game>::new(config, SeatId(1));
        host.add_seat(SeatId(1));
        host.start(0);
        // Ten seconds of frames arrive and the application never calls `next_frame`.
        for now in 0..10_000u64 {
            if now.is_multiple_of(16) {
                for out in seat.outgoing(now) {
                    host.receive(SeatId(1), out.lane, &out.bytes, now)
                        .expect("seat payload");
                }
                while host.poll_frame(now).is_some() {}
                for (_, out) in host.outgoing(now) {
                    seat.receive(out.lane, &out.bytes, now).expect("host payload");
                }
            }
        }
        assert!(
            seat.frames_waiting() <= 64 + 256,
            "budget {budget}: {} frames queued",
            seat.frames_waiting()
        );
        assert!(
            seat.frames_received_through() < 400,
            "budget {budget}: the rest stays unacknowledged at the host"
        );
        assert!(
            host.status(10_000).seats[0].lagging,
            "budget {budget}: the host stopped feeding a seat that does not step"
        );
        // Once it steps again it gets every frame it took, in order, from where it stopped:
        // nothing the stream delivered was dropped.
        let mut ticks = Vec::new();
        while let Some(frame) = seat.next_frame() {
            ticks.push(frame.tick);
        }
        assert!(ticks.len() >= 64, "budget {budget}: {} frames", ticks.len());
        assert!(ticks.iter().copied().eq(0..ticks.len() as u64), "budget {budget}");
        assert_eq!(seat.frames_received_through(), ticks.len() as u64);
    }
}

#[test]
fn a_leave_applies_on_the_same_tick_for_every_seat() {
    let mut rig = Rig::new(
        config(),
        HostRole::Listen(SeatId(0)),
        &[1, 2, 3],
        LinkSpec::latency(10),
    );
    rig.run_ms(1_000);
    rig.host.remove_seat(SeatId(2));
    rig.run_ms(1_000);
    let leave: Vec<&LockstepFrame<Pad>> = rig
        .frames
        .iter()
        .filter(|f| !f.membership.is_empty() && f.tick > 0)
        .collect();
    assert_eq!(leave.len(), 1);
    assert_eq!(
        leave[0].membership,
        [MembershipChange {
            seat: SeatId(2),
            kind: MembershipKind::Leave
        }]
    );
    let tick = leave[0].tick;
    for frame in &rig.frames {
        let listed = frame.inputs.iter().any(|(s, _)| *s == SeatId(2));
        assert_eq!(listed, frame.tick < tick, "tick {}", frame.tick);
    }
    // The seats that stay agree on it (their simulations fold the seat list).
    let stay: Vec<u64> = [1, 3]
        .iter()
        .map(|s| rig.seats[&SeatId(*s)].sim.hashes[&(tick + 20)])
        .collect();
    assert_eq!(stay[0], stay[1]);
    assert_eq!(
        Some(stay[0]),
        rig.host_sim.as_ref().map(|s| s.hashes[&(tick + 20)])
    );
}

// ---- hash authority by quorum ---------------------------------------------------------------

fn no_self_check() -> LockstepConfig {
    LockstepConfig {
        self_check: false,
        ..config()
    }
}

#[test]
fn drifted_host_is_outvoted_and_resynced_not_the_joiners() {
    let mut rig = Rig::new(
        no_self_check(),
        HostRole::Listen(SeatId(0)),
        &[1, 2, 3],
        LinkSpec::latency(10),
    );
    rig.host_drift_at = Some(100);
    rig.host_drift_sticky = true;
    rig.run_ms(6_000);
    let verdicts = rig.desyncs();
    assert_eq!(verdicts.len(), 1, "{verdicts:?}");
    let v = verdicts[0];
    assert_eq!(v.kind, VerdictKind::Diverged);
    assert_eq!(v.tick, 120, "the first checkpoint after the drift at tick 100");
    assert_eq!(v.last_agreed, Some(90));
    assert!(v.host_diverged);
    assert_eq!(v.diverged, [Voter::Seat(SeatId(0))], "only the host");
    assert_eq!(
        v.source,
        Some(Voter::Seat(SeatId(1))),
        "a majority seat supplies the state"
    );
    // The host is resynced; no joiner is resynced, removed or told to.
    assert!(rig.host_events.iter().any(|e| matches!(
        e,
        HostEvent::ResyncHost {
            source: Voter::Seat(SeatId(1)),
            tick: 120,
            resyncs: 1,
            counted: true
        }
    )));
    assert!(
        !rig.host_events
            .iter()
            .any(|e| matches!(e, HostEvent::ResyncSeat { .. } | HostEvent::SeatRemoved { .. }))
    );
    for (id, node) in &rig.seats {
        assert!(
            !node
                .events
                .iter()
                .any(|e| matches!(e, SeatEvent::Resync { .. } | SeatEvent::Removed { .. })),
            "{id} was touched"
        );
    }
    // The majority seat was asked for its state.
    assert!(rig.seats[&SeatId(1)].events.contains(&SeatEvent::ProvideState {
        tick: 120,
        target: Voter::Seat(SeatId(0))
    }));
    // The probe names the tick the host's hashes first differ at.
    assert!(rig.host_events.contains(&HostEvent::DesyncLocated {
        tick: 120,
        voter: Voter::Seat(SeatId(0)),
        first_diverged_tick: 100
    }));
    // After the transfer everyone is in step again, all four seats are still listed, and the
    // host is still the frame authority.
    rig.assert_in_step();
    let status = rig.host.status(rig.now);
    assert_eq!((status.host_resyncs, status.desyncs), (1, 1));
    assert_eq!(rig.host.members().count(), 4);
    assert!(
        status
            .seats
            .iter()
            .all(|s| !s.resyncing && s.resyncs == u32::from(s.seat == 0))
    );
    assert!(status.last_agreed_tick.is_some_and(|t| t >= 300));
}

#[test]
fn a_two_seat_split_is_a_tie_that_removes_nobody() {
    // A listen host and one joiner: two voters, so a differing hash is one against one.
    let mut rig = Rig::new(
        no_self_check(),
        HostRole::Listen(SeatId(0)),
        &[1],
        LinkSpec::latency(10),
    );
    rig.seats.get_mut(&SeatId(1)).expect("joiner").drift_at = Some(100);
    rig.seats.get_mut(&SeatId(1)).expect("joiner").drift_sticky = true;
    rig.run_ms(6_000);
    let verdicts = rig.desyncs();
    assert_eq!(verdicts.len(), 1);
    assert_eq!(
        verdicts[0].kind,
        VerdictKind::Split,
        "no strict majority: nobody is convicted"
    );
    assert!(!verdicts[0].host_diverged);
    // The stated rule: the host's group supplies the state so the match goes on; it is counted as
    // a tie, never against the seat, and the seat stays.
    assert!(rig.host_events.iter().any(|e| matches!(
        e,
        HostEvent::ResyncSeat {
            seat: SeatId(1),
            counted: false,
            resyncs: 0,
            ..
        }
    )));
    assert!(
        !rig.host_events
            .iter()
            .any(|e| matches!(e, HostEvent::SeatRemoved { .. }))
    );
    rig.assert_in_step();
    let status = rig.host.status(rig.now);
    assert_eq!((status.ties, status.seats[1].resyncs), (1, 0));
    assert_eq!(rig.host.members().count(), 2);

    // The same split with the host as the one that drifted is the same tie: the joiner is not
    // removed for the host's fault either.
    let mut rig = Rig::new(
        no_self_check(),
        HostRole::Listen(SeatId(0)),
        &[1],
        LinkSpec::latency(10),
    );
    rig.host_drift_at = Some(100);
    rig.host_drift_sticky = true;
    rig.run_ms(6_000);
    assert_eq!(rig.desyncs()[0].kind, VerdictKind::Split);
    assert!(
        !rig.host_events
            .iter()
            .any(|e| matches!(e, HostEvent::SeatRemoved { .. }))
    );
    assert_eq!(rig.host.members().count(), 2);

    // With the self-check on, a one-off fault on either side repairs itself: no transfer at all.
    for host_drifts in [true, false] {
        let mut rig = Rig::new(config(), HostRole::Listen(SeatId(0)), &[1], LinkSpec::latency(10));
        if host_drifts {
            rig.host_drift_at = Some(100);
        } else {
            rig.seats.get_mut(&SeatId(1)).expect("joiner").drift_at = Some(100);
        }
        rig.run_ms(6_000);
        let verdicts = rig.desyncs();
        assert_eq!(verdicts.len(), 1);
        assert_eq!(verdicts[0].kind, VerdictKind::SelfHealed);
        let healed = if host_drifts { SeatId(0) } else { SeatId(1) };
        assert_eq!(verdicts[0].self_healed, [Voter::Seat(healed)]);
        assert!(
            !rig.host_events
                .iter()
                .any(|e| matches!(e, HostEvent::ResyncSeat { .. } | HostEvent::ResyncHost { .. }))
        );
        rig.assert_in_step();
    }
}

#[test]
fn a_tie_under_end_match_ends_unresolved_without_blame() {
    let config = LockstepConfig {
        tie_policy: TiePolicy::EndMatch,
        ..no_self_check()
    };
    let mut rig = Rig::new(config, HostRole::Listen(SeatId(0)), &[1], LinkSpec::latency(10));
    rig.host_drift_at = Some(100);
    rig.host_drift_sticky = true;
    rig.run_ms(4_000);
    assert!(rig.host_events.contains(&HostEvent::Unresolved { tick: 120 }));
    assert!(rig.host.unresolved());
    assert!(
        rig.seats[&SeatId(1)]
            .events
            .contains(&SeatEvent::Unresolved { tick: 120 })
    );
    assert!(
        !rig.host_events
            .iter()
            .any(|e| matches!(e, HostEvent::SeatRemoved { .. }))
    );
}

#[test]
fn a_planted_single_seat_drift_resyncs_only_that_seat() {
    let mut rig = Rig::new(
        no_self_check(),
        HostRole::Listen(SeatId(0)),
        &[1, 2, 3],
        LinkSpec::latency(10),
    );
    rig.seats.get_mut(&SeatId(2)).expect("seat 2").drift_at = Some(131);
    rig.seats.get_mut(&SeatId(2)).expect("seat 2").drift_sticky = true;
    rig.run_ms(6_000);
    let verdicts = rig.desyncs();
    assert_eq!(verdicts.len(), 1);
    let v = verdicts[0];
    assert_eq!(
        (v.kind, v.tick, v.last_agreed),
        (VerdictKind::Diverged, 150, Some(120))
    );
    assert_eq!(v.diverged, [Voter::Seat(SeatId(2))], "the right seat");
    assert!(!v.host_diverged);
    assert!(rig.host_events.contains(&HostEvent::DesyncLocated {
        tick: 150,
        voter: Voter::Seat(SeatId(2)),
        first_diverged_tick: 131
    }));
    let resyncs: Vec<&HostEvent> = rig
        .host_events
        .iter()
        .filter(|e| matches!(e, HostEvent::ResyncSeat { .. } | HostEvent::ResyncHost { .. }))
        .collect();
    assert_eq!(
        resyncs,
        [&HostEvent::ResyncSeat {
            seat: SeatId(2),
            source: Voter::Seat(SeatId(0)),
            tick: 150,
            resyncs: 1,
            counted: true
        }]
    );
    for id in [1, 3] {
        assert!(
            !rig.seats[&SeatId(id)]
                .events
                .iter()
                .any(|e| matches!(e, SeatEvent::Resync { .. })),
            "seat {id} was not touched"
        );
    }
    rig.assert_in_step();
    let status = rig.host.status(rig.now);
    assert!(status.seats.iter().all(|s| !s.resyncing));
    assert_eq!(rig.host.members().count(), 4);
}

#[test]
fn a_seat_that_keeps_drifting_is_removed_alone_after_the_resync_limit() {
    let mut rig = Rig::new(
        no_self_check(),
        HostRole::Listen(SeatId(0)),
        &[1, 2, 3],
        LinkSpec::latency(10),
    );
    for drift in [100u64, 400, 700] {
        let node = rig.seats.get_mut(&SeatId(3)).expect("seat 3");
        node.drift_at = Some(drift);
        node.drift_sticky = true;
        rig.run_ms(5_000);
    }
    let removed: Vec<&HostEvent> = rig
        .host_events
        .iter()
        .filter(|e| matches!(e, HostEvent::SeatRemoved { .. }))
        .collect();
    assert_eq!(removed.len(), 1, "{removed:?}");
    assert!(matches!(
        removed[0],
        HostEvent::SeatRemoved {
            seat: SeatId(3),
            reason: "desync",
            ..
        }
    ));
    assert!(rig.seats[&SeatId(3)].seat.removed());
    let members: Vec<SeatId> = rig.host.members().collect();
    assert_eq!(
        members,
        [SeatId(0), SeatId(1), SeatId(2)],
        "the match goes on for everyone else"
    );
    assert_eq!(rig.desyncs().len(), 3);
}

#[test]
fn a_dedicated_observer_is_one_vote_and_a_relay_tallies_only_the_seats() {
    // An observer host whose own simulation drifts is outvoted by its two seats.
    let mut rig = Rig::new(
        no_self_check(),
        HostRole::Observer,
        &[1, 2],
        LinkSpec::latency(10),
    );
    rig.host_drift_at = Some(100);
    rig.host_drift_sticky = true;
    rig.run_ms(5_000);
    let verdicts = rig.desyncs();
    assert_eq!(verdicts.len(), 1);
    assert!(verdicts[0].host_diverged);
    assert_eq!(verdicts[0].diverged, [Voter::Observer]);
    assert!(
        !rig.host_events
            .iter()
            .any(|e| matches!(e, HostEvent::ResyncSeat { .. }))
    );

    // A relay has no vote: three seats decide among themselves.
    let mut rig = Rig::new(
        no_self_check(),
        HostRole::Relay,
        &[1, 2, 3],
        LinkSpec::latency(10),
    );
    rig.seats.get_mut(&SeatId(1)).expect("seat 1").drift_at = Some(100);
    rig.seats.get_mut(&SeatId(1)).expect("seat 1").drift_sticky = true;
    rig.run_ms(5_000);
    let verdicts = rig.desyncs();
    assert_eq!(verdicts.len(), 1);
    assert_eq!(verdicts[0].diverged, [Voter::Seat(SeatId(1))]);
    assert_eq!(
        verdicts[0].source,
        Some(Voter::Seat(SeatId(2))),
        "the lowest majority seat"
    );
}

#[test]
fn the_status_reports_delay_frame_lag_and_resyncs() {
    let mut rig = Rig::new(
        config(),
        HostRole::Listen(SeatId(0)),
        &[1, 2],
        LinkSpec::latency(40),
    );
    rig.run_ms(5_000);
    let status = rig.host.status(rig.now);
    assert_eq!(status.role, "host");
    assert!(
        status.delay_ticks >= 5,
        "80 ms round trips: {}",
        status.delay_ticks
    );
    assert_eq!(status.seats[0].frame_lag_ticks, 0, "the host's own seat");
    for seat in &status.seats[1..] {
        assert!(
            (4..=8).contains(&seat.frame_lag_ticks),
            "about a round trip of frames in flight: {seat:?}"
        );
        assert!(seat.lateness_p99_us.is_some_and(|l| l > 60_000));
        assert!(seat.delay_need_ticks.is_some());
    }
    assert!(status.datagrams_sent > 500 && status.datagram_bytes_sent > 0);
    let json = serde_json_like(&status);
    assert!(json.contains("frame_lag_ticks") && json.contains("delay_ticks"));
    let seat = rig.seats[&SeatId(1)].seat.status();
    assert_eq!(seat.role, "seat");
    assert_eq!(seat.delay_ticks, status.delay_ticks);
}

/// The status serializes (the agent protocol sends it as JSON); RON is what this crate links.
fn serde_json_like(status: &LockstepStatus) -> String {
    ron::to_string(status).expect("status serializes")
}

// ---- found by review after the first green run (ADR-8146): stream runs, hitches, re-admission,
// ---- extreme values ------------------------------------------------------------------------

/// A relay host and one seat driven by hand at 50 Hz (20 ms a tick), so a test decides the fate
/// of every payload.
struct Pair {
    host: LockstepHost<Game>,
    seat: LockstepSeat<Game>,
    now: u64,
    stepped: Vec<u64>,
    input_of: fn(u64) -> Pad,
}

impl Pair {
    fn new(config: LockstepConfig) -> Self {
        let mut host = LockstepHost::<Game>::new(config.clone(), HostRole::Relay);
        host.add_seat(SeatId(1));
        host.start(0);
        Self {
            host,
            seat: LockstepSeat::new(config, SeatId(1)),
            now: 0,
            stepped: Vec::new(),
            input_of: |capture| steady_input(SeatId(1), capture),
        }
    }

    /// The seat samples and sends, the host builds what is due: what the host sends in return.
    fn up_and_build(&mut self) -> Vec<Outgoing> {
        let capture = self.stepped.len() as u64;
        // Refused while the seat waits for a frame: it has sampled this capture already.
        let _ = self.seat.submit_input(&(self.input_of)(capture));
        for o in self.seat.outgoing(self.now) {
            self.host
                .receive(SeatId(1), o.lane, &o.bytes, self.now)
                .expect("the seat's payload");
        }
        while self.host.poll_frame(self.now).is_some() {}
        self.host.outgoing(self.now).into_iter().map(|(_, o)| o).collect()
    }

    fn deliver(&mut self, out: &Outgoing) {
        self.seat
            .receive(out.lane, &out.bytes, self.now)
            .expect("the host's payload");
        while let Some(frame) = self.seat.next_frame() {
            self.stepped.push(frame.tick);
        }
    }

    /// One tick with every payload delivered at once.
    fn tick(&mut self) {
        for o in self.up_and_build() {
            self.deliver(&o);
        }
        self.now += 20;
    }

    fn assert_stepped_in_order_to(&self, last: u64) {
        let want: Vec<u64> = (0..=last).collect();
        assert_eq!(
            self.stepped.len(),
            want.len(),
            "stepped through {:?}, want through {last}",
            self.stepped.last()
        );
        assert_eq!(self.stepped, want, "every tick once, in order");
    }
}

fn only_datagram(out: &[Outgoing]) -> &Outgoing {
    let mut datagrams = out.iter().filter(|o| o.lane == Lane::Datagram);
    let first = datagrams.next().expect("a frames datagram");
    assert!(datagrams.next().is_none(), "one frames datagram a tick");
    first
}

/// The frames of an oversize window go on the stream, which delivers once and in order: the run
/// has to begin at a frame the seat is known to have reached. A run that began at the window's
/// start left a seat that had lost the datagrams before it with a payload it could not apply,
/// and once a late datagram moved its acknowledgement into the run the host never sent that
/// frame again.
#[test]
fn a_stream_run_starts_at_the_acknowledged_frame_so_a_late_datagram_cannot_strand_the_seat() {
    const B: u64 = 30;
    let config = LockstepConfig {
        datagram_budget_bytes: 300,
        ..LockstepConfig::at_rate(50)
    };
    let delay = u64::from(config.initial_delay_ticks);
    assert_eq!(delay, 4, "the captures below are stamped for ticks B and B + 1");
    let mut pair = Pair::new(config);
    // Frames B and B + 1 each carry a new 180-byte input: either fits a datagram, both do not.
    pair.input_of = |capture| Pad {
        blob: if capture == B - 4 || capture == B - 3 {
            (0..180u64).map(|i| (i * 7 + capture) as u8).collect()
        } else {
            Vec::new()
        },
        ..steady_input(SeatId(1), capture)
    };
    for _ in 0..B - 1 {
        pair.tick();
    }
    pair.assert_stepped_in_order_to(B - 2);

    // Tick B - 1: its datagram is delayed.
    let out = pair.up_and_build();
    assert!(out.iter().all(|o| o.lane == Lane::Datagram), "no stream yet");
    let late = only_datagram(&out).clone();
    pair.now += 20;
    // Tick B: its datagram is lost.
    let out = pair.up_and_build();
    assert!(out.iter().all(|o| o.lane == Lane::Datagram), "frame B alone fits");
    pair.now += 20;
    // Tick B + 1: frames B and B + 1 do not fit together, so the older goes on the stream.
    let out = pair.up_and_build();
    assert!(
        out.iter().any(|o| o.lane == Lane::Stream),
        "the oversize window uses the stream"
    );
    for o in out.iter().filter(|o| o.lane == Lane::Stream) {
        pair.deliver(o);
    }
    pair.deliver(only_datagram(&out));
    // The delayed datagram arrives after the stream payload.
    pair.deliver(&late);
    pair.now += 20;

    for _ in 0..40 {
        pair.tick();
    }
    let built = pair.host.tick();
    pair.assert_stepped_in_order_to(built - 1);
    while let Some(event) = pair.host.poll_event() {
        assert!(
            !matches!(event, HostEvent::SeatLagging { .. }),
            "the seat never fell behind: {event:?}"
        );
    }
}

/// A host that builds several frames between two sends (a hitch, or a listen host that renders
/// slower than it ticks) still sends every one of them: the redundancy window reaches back to
/// the first frame that was never sent, not only a fixed depth from the newest.
#[test]
fn frames_built_between_two_sends_are_all_in_the_next_datagram() {
    let mut pair = Pair::new(LockstepConfig::at_rate(50));
    for _ in 0..20 {
        pair.tick();
    }
    pair.assert_stepped_in_order_to(19);
    // Two ticks pass without a send: frames 20, 21 and 22 are due at once.
    pair.now += 40;
    let out = pair.up_and_build();
    assert_eq!(pair.host.tick(), 23);
    let datagram = only_datagram(&out).clone();
    let (header, bodies) = wire::decode_frames_datagram(&datagram.bytes).expect("decodes");
    assert_eq!(
        (header.base_tick, bodies.len()),
        (20, 3),
        "the datagram starts at the first frame never sent"
    );
    pair.deliver(&datagram);
    pair.assert_stepped_in_order_to(22);
    // No hole was reported: the seat's next datagram asks for nothing.
    pair.now += 20;
    let _ = pair.seat.submit_input(&steady_input(SeatId(1), 23));
    let up = pair.seat.outgoing(pair.now);
    let inputs = up
        .iter()
        .find(|o| o.lane == Lane::Datagram)
        .expect("an inputs datagram");
    let msg = wire::decode_inputs(&inputs.bytes, 1024).expect("decodes");
    assert!(!msg.hole && msg.ack_next == 23, "{msg:?}");
}

/// Removing a seat and adding it again before the next frame is the same as doing it a tick
/// apart: the leave is listed on one frame, the join on the next, and the seat that joined is
/// served like any other (it used to inherit the leave and be dropped while still a member).
#[test]
fn a_seat_removed_and_added_again_before_the_next_frame_leaves_then_joins_and_stays_served() {
    let mut host = LockstepHost::<Game>::new(LockstepConfig::at_rate(50), HostRole::Relay);
    host.add_seat(SeatId(1));
    host.add_seat(SeatId(2));
    host.start(0);
    let mut now = 0;
    for _ in 0..5 {
        while host.poll_frame(now).is_some() {}
        host.outgoing(now);
        now += 20;
    }
    host.remove_seat(SeatId(2));
    host.add_seat(SeatId(2));
    let leave = host.poll_frame(now).expect("tick 5 is due");
    now += 20;
    let join = host.poll_frame(now).expect("tick 6 is due");
    let change = |kind| MembershipChange {
        seat: SeatId(2),
        kind,
    };
    assert_eq!(leave.membership, vec![change(MembershipKind::Leave)]);
    assert!(leave.inputs.iter().all(|(seat, _)| *seat != SeatId(2)));
    assert_eq!(join.membership, vec![change(MembershipKind::Join)]);
    assert!(join.inputs.iter().any(|(seat, _)| *seat == SeatId(2)));

    // Removing it once more before the join's frame cancels only the join.
    host.remove_seat(SeatId(1));
    host.add_seat(SeatId(1));
    host.remove_seat(SeatId(1));
    now += 20;
    let frame = host.poll_frame(now).expect("tick 7 is due");
    assert_eq!(
        frame.membership,
        vec![MembershipChange {
            seat: SeatId(1),
            kind: MembershipKind::Leave
        }]
    );
    now += 20;
    let frame = host.poll_frame(now).expect("tick 8 is due");
    assert!(frame.membership.is_empty(), "the cancelled join is not listed");

    // Long after a seat that left would have been forgotten, the seat that joined again is
    // still a member the host listens to.
    for _ in 0..700 {
        now += 20;
        while host.poll_frame(now).is_some() {}
        host.outgoing(now);
    }
    assert_eq!(host.members().collect::<Vec<_>>(), vec![SeatId(2)]);
    let hello = wire::encode_inputs(&wire::InputsMsg {
        seq: 0,
        ack_next: host.tick(),
        down_p99_run: 0,
        down_window_run: 0,
        lead: 0,
        hole: false,
        leave: false,
        catching_up: false,
        votes: Vec::new(),
        first_tick: host.tick(),
        inputs: Vec::new(),
    });
    assert_eq!(host.receive(SeatId(2), Lane::Datagram, &hello, now), Ok(()));
    assert_eq!(
        host.receive(SeatId(1), Lane::Datagram, &hello, now),
        Err(LockstepError::UnknownSeat(SeatId(1))),
        "the seat that left is forgotten"
    );
}

/// A vote names a checkpoint the seat stepped, so the host built it first. A vote for a tick
/// the host has not built is not acknowledged (the acknowledgement is sent as `tick + 1`).
#[test]
fn a_vote_for_a_tick_the_host_has_not_built_is_not_acknowledged() {
    let mut host = LockstepHost::<Game>::new(config(), HostRole::Relay);
    host.add_seat(SeatId(1));
    host.start(0);
    while host.poll_frame(200).is_some() {}
    let vote = |tick: u64| {
        wire::encode_inputs(&wire::InputsMsg {
            seq: tick % 7,
            ack_next: 0,
            down_p99_run: 0,
            down_window_run: 0,
            lead: 0,
            hole: false,
            leave: false,
            catching_up: false,
            votes: vec![(tick, 7)],
            first_tick: 0,
            inputs: Vec::new(),
        })
    };
    let vote_ack = |host: &mut LockstepHost<Game>| {
        let out = host.outgoing(200);
        let (_, datagram) = out
            .iter()
            .find(|(_, o)| o.lane == Lane::Datagram)
            .expect("a frames datagram");
        let (header, _) = wire::decode_frames_datagram(&datagram.bytes).expect("decodes");
        header.vote_ack
    };
    host.receive(SeatId(1), Lane::Datagram, &vote(u64::MAX), 200)
        .expect("well formed");
    assert_eq!(vote_ack(&mut host), None, "nothing the host built was voted on");
    let built = host.tick();
    host.receive(SeatId(1), Lane::Datagram, &vote(built), 200)
        .expect("well formed");
    assert_eq!(vote_ack(&mut host), None, "tick {built} is not built yet");
    host.receive(SeatId(1), Lane::Datagram, &vote(built - 1), 200)
        .expect("well formed");
    assert_eq!(vote_ack(&mut host), Some(built - 1));
}

/// The decoders bound what they read; the state machines behind them must hold too. Every
/// field a peer controls is driven to the ends of its range, against a host and a seat that
/// keep running.
#[test]
fn extreme_values_from_the_wire_never_panic_the_host_or_the_seat() {
    const EDGE: [u64; 8] = [0, 1, 2, 255, 1 << 32, u64::MAX / 2, u64::MAX - 1, u64::MAX];
    fn pick(rng: &mut Rng, near: u64) -> u64 {
        match rng.below(4) {
            0 => EDGE[rng.below(8) as usize],
            1 => near.wrapping_add(rng.below(9)).wrapping_sub(4),
            2 => rng.next(),
            _ => near,
        }
    }
    let mut rng = Rng(0xfeed_5eed);
    let mut host = LockstepHost::<Game>::new(config(), HostRole::Listen(SeatId(0)));
    host.add_seat(SeatId(0));
    host.add_seat(SeatId(1));
    host.start(0);
    let mut seat = LockstepSeat::<Game>::new(config(), SeatId(1));
    let pad = Pad::default().to_bytes().expect("encodes");
    let mut now = 0u64;
    for round in 0..3_000u64 {
        now += rng.below(40);
        let tick = host.tick();
        let msg = wire::InputsMsg {
            seq: pick(&mut rng, round),
            ack_next: pick(&mut rng, tick),
            down_p99_run: pick(&mut rng, 3) as u32,
            down_window_run: pick(&mut rng, 3) as u32,
            lead: pick(&mut rng, 2) as u16,
            hole: rng.chance(200),
            leave: false,
            catching_up: false,
            votes: (0..rng.below(4))
                .map(|_| (pick(&mut rng, tick), rng.next()))
                .collect(),
            first_tick: pick(&mut rng, tick),
            inputs: (0..rng.below(6))
                .map(|_| rng.chance(700).then(|| pad.clone()))
                .collect(),
        };
        let _ = host.receive(SeatId(1), Lane::Datagram, &wire::encode_inputs(&msg), now);
        let _ = host.local_input(&Pad::default(), now);
        while host.poll_frame(now).is_some() {}
        for (_, o) in host.outgoing(now) {
            let _ = seat.receive(o.lane, &o.bytes, now);
        }
        // A frames datagram from a host that fills its header with anything.
        let header = wire::FramesHeader {
            seq: pick(&mut rng, round),
            next_build: pick(&mut rng, tick),
            vote_ack: rng.chance(500).then(|| pick(&mut rng, tick).min(u64::MAX - 1)),
            base_tick: pick(&mut rng, tick),
            feedback: None,
        };
        let empty = wire::encode_frames_datagram(&header, 0, std::iter::empty());
        let _ = seat.receive(Lane::Datagram, &empty, now);
        let _ = seat.submit_input(&Pad::default());
        while seat.next_frame().is_some() {}
        let _ = seat.outgoing(now);
        while host.poll_event().is_some() {}
        while seat.poll_event().is_some() {}
        let _ = host.status(now);
        let _ = seat.status();
    }
    assert!(host.tick() > 1_000, "the host kept building: {}", host.tick());
}

/// The stall a single lost datagram causes is measured even when the next datagram does not
/// repair it: the count and the worst stall close when the frame the seat lacked has arrived.
/// (The metric used to leave such a loss out, so the two-tick bar could not fail.)
#[test]
fn a_single_loss_the_next_datagram_does_not_repair_is_counted_with_its_whole_stall() {
    let mut pair = Pair::new(LockstepConfig::at_rate(50));
    for _ in 0..10 {
        pair.tick();
    }
    pair.assert_stepped_in_order_to(9);
    // Tick 10's datagram is lost.
    let _lost = pair.up_and_build();
    pair.now += 20;
    // Tick 11's datagram arrives cut down to its newest frame: it does not reach back.
    let out = pair.up_and_build();
    let whole = only_datagram(&out).clone();
    let (header, bodies) = wire::decode_frames_datagram(&whole.bytes).expect("decodes");
    assert_eq!((header.base_tick, bodies.len()), (10, 2));
    let cut = wire::encode_frames_datagram(
        &wire::FramesHeader {
            base_tick: 11,
            ..header.clone()
        },
        1,
        bodies[1..].iter().copied(),
    );
    pair.seat
        .receive(Lane::Datagram, &cut, pair.now)
        .expect("well formed");
    assert!(pair.seat.next_frame().is_none(), "frame 10 is missing");
    assert_eq!(pair.seat.status().single_losses, 0, "not repaired yet");
    pair.now += 20;
    // The seat reports the hole and the host answers with every unacknowledged frame.
    pair.tick();
    pair.tick();
    let status = pair.seat.status();
    assert_eq!(status.single_losses, 1, "the loss is counted once it is repaired");
    assert_eq!(
        status.single_loss_repair_ticks_max, 2,
        "frame 10 arrived with frame 12: a two-tick stall"
    );
    pair.assert_stepped_in_order_to(pair.host.tick() - 1);
    // A loss the next datagram repairs is a one-tick stall.
    let _lost = pair.up_and_build();
    pair.now += 20;
    pair.tick();
    let status = pair.seat.status();
    assert_eq!(
        (status.single_losses, status.single_loss_repair_ticks_max),
        (2, 2)
    );
    pair.assert_stepped_in_order_to(pair.host.tick() - 1);
}

/// Under `PerSeat` a seat that stamps with its own lead has inputs on the ticks a raise of the
/// shared delay leaves empty for everyone else. They were stamped before the raise, and they
/// are in their frames (they used to be dropped with the gap).
#[test]
fn per_seat_inputs_stamped_before_a_raise_are_in_the_frames_the_raise_leaves_empty() {
    let config = LockstepConfig {
        delay_policy: DelayPolicy::PerSeat,
        ..LockstepConfig::at_rate(50)
    };
    let delay = u64::from(config.initial_delay_ticks);
    let mut host = LockstepHost::<Game>::new(config, HostRole::Relay);
    for seat in 1..=3 {
        host.add_seat(SeatId(seat));
    }
    host.start(0);
    let pad = |tick: u64| Pad {
        aim: tick as u32,
        ..Pad::default()
    };
    let inputs = |seq: u64, lead: u16, tick: u64, next: u64| {
        wire::encode_inputs(&wire::InputsMsg {
            seq,
            ack_next: next,
            down_p99_run: 0,
            down_window_run: 0,
            lead,
            hole: false,
            leave: false,
            catching_up: false,
            votes: Vec::new(),
            first_tick: tick,
            inputs: vec![Some(pad(tick).to_bytes().expect("encodes"))],
        })
    };
    let mut frames: Vec<LockstepFrame<Pad>> = Vec::new();
    for t in 0..200u64 {
        let now = t * 20;
        // Seat 3 stamps two ticks further ahead than the frame delay and is always on time.
        host.receive(SeatId(3), Lane::Datagram, &inputs(t, 2, t + delay + 2, t), now)
            .expect("seat 3");
        // Seats 1 and 2 are on time for a second, then a tick late: the delay has to rise.
        let tick = if t < 50 { t + delay } else { t - 1 };
        for seat in [1u16, 2] {
            host.receive(SeatId(seat), Lane::Datagram, &inputs(t, 0, tick, t), now)
                .expect("seats 1 and 2");
        }
        while let Some(frame) = host.poll_frame(now) {
            frames.push(frame);
        }
        host.outgoing(now);
    }
    let raised = frames
        .iter()
        .find(|f| u64::from(f.delay) > delay)
        .expect("the late seats raised the delay");
    let (from, new_delay) = (raised.delay_from, u64::from(raised.delay));
    assert!(from > 50, "the raise follows the late inputs: {from}");
    // The shared schedule leaves these ticks empty: no capture of a seat without a lead maps here.
    let empty: Vec<u64> = (from + delay..from + new_delay).collect();
    assert!(!empty.is_empty());
    for tick in empty {
        let frame = &frames[tick as usize];
        assert_eq!(frame.tick, tick);
        let of = |seat: u16| {
            frame
                .inputs
                .iter()
                .find(|(s, _)| *s == SeatId(seat))
                .map(|(_, input)| input.clone())
        };
        assert_eq!(
            of(3),
            Some(SeatInput::Present(pad(tick))),
            "tick {tick}: stamped with the seat's own lead before the raise"
        );
        assert_eq!(of(1), Some(SeatInput::Absent), "tick {tick}");
        assert_eq!(of(2), Some(SeatInput::Absent), "tick {tick}");
    }
}

// ---- second pass (ADR-8209): the review items the first pass left open -------------------------

fn resyncs_of(events: &[HostEvent], seat: SeatId) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, HostEvent::ResyncSeat { seat: s, .. } if *s == seat))
        .count()
}

/// With a checkpoint every tick the newest reference has moved on by the time a resynced seat's
/// vote arrives. A vote that equals the reference of its transfer's checkpoint still takes the
/// seat back: it used to stay ignored until the transfer's deadline, was ordered to resync again
/// and again, and was removed for a divergence it had repaired.
#[test]
fn with_a_checkpoint_every_tick_a_resynced_seat_is_taken_back_and_not_resynced_again() {
    for hash_every in [1u32, 2] {
        let config = LockstepConfig {
            hash_every,
            self_check: false,
            ..config()
        };
        let mut rig = Rig::new(config, HostRole::Observer, &[1, 2, 3], LinkSpec::latency(20));
        // Seat 3 is the slow one: its votes always arrive after the others closed the checkpoint.
        let node = rig.seats.get_mut(&SeatId(3)).expect("seat 3");
        node.down.spec = LinkSpec::latency(60);
        node.up.spec = LinkSpec::latency(60);
        node.drift_at = Some(100);
        rig.run_ms(9_000);
        assert_eq!(
            resyncs_of(&rig.host_events, SeatId(3)),
            1,
            "hash_every {hash_every}: one transfer repairs the seat"
        );
        assert!(
            !rig.host_events
                .iter()
                .any(|e| matches!(e, HostEvent::SeatRemoved { .. } | HostEvent::Unresolved { .. })),
            "hash_every {hash_every}: nobody is removed for a divergence that was repaired"
        );
        let status = rig.host.status(rig.now);
        let row = status.seats.iter().find(|s| s.seat == 3).expect("seat 3");
        assert!(
            !row.resyncing && row.resyncs == 1,
            "hash_every {hash_every}: {row:?}"
        );
        rig.assert_in_step();
    }
}

/// A seat that is added again while its previous holder has not acknowledged the frame that lists
/// its leave: the host hands the frames the previous holder still lacks to the application, which
/// sends them to the connection that held the seat. The previous holder steps its own leave.
#[test]
fn the_previous_holder_of_a_seat_added_again_gets_the_frame_that_lists_its_leave() {
    // `apart`: the seat is added again in the same tick, or two ticks after the leave's frame.
    for apart in [false, true] {
        let config = LockstepConfig::at_rate(50);
        let mut host = LockstepHost::<Game>::new(config.clone(), HostRole::Relay);
        host.add_seat(SeatId(1));
        host.add_seat(SeatId(2));
        host.start(0);
        let mut old = LockstepSeat::<Game>::new(config, SeatId(2));
        let mut stepped: Vec<LockstepFrame<Pad>> = Vec::new();
        let mut now = 0u64;
        for _ in 0..5 {
            let _ = old.submit_input(&steady_input(SeatId(2), stepped.len() as u64));
            for o in old.outgoing(now) {
                host.receive(SeatId(2), o.lane, &o.bytes, now)
                    .expect("seat payload");
            }
            while host.poll_frame(now).is_some() {}
            for (to, o) in host.outgoing(now) {
                if to == SeatId(2) {
                    old.receive(o.lane, &o.bytes, now).expect("host payload");
                }
            }
            while let Some(frame) = old.next_frame() {
                stepped.push(frame);
            }
            now += 20;
        }
        assert_eq!(stepped.len(), 5);
        while host.poll_event().is_some() {}

        // The seat changes hands. Every datagram to the previous holder from here on is lost.
        host.remove_seat(SeatId(2));
        if !apart {
            host.add_seat(SeatId(2));
        }
        let leave = host.poll_frame(now).expect("tick 5 is due");
        assert_eq!(
            leave.membership,
            vec![MembershipChange {
                seat: SeatId(2),
                kind: MembershipKind::Leave
            }]
        );
        host.outgoing(now);
        if apart {
            for _ in 0..2 {
                now += 20;
                while host.poll_frame(now).is_some() {}
                host.outgoing(now);
            }
            host.add_seat(SeatId(2));
        }
        let mut handover = None;
        while let Some(event) = host.poll_event() {
            if let HostEvent::SeatHandover {
                seat,
                leave_tick,
                payloads,
            } = event
            {
                assert!(handover.is_none(), "one handover");
                handover = Some((seat, leave_tick, payloads));
            }
        }
        let (seat, leave_tick, payloads) =
            handover.unwrap_or_else(|| panic!("apart {apart}: the previous holder is owed its leave frame"));
        assert_eq!((seat, leave_tick), (SeatId(2), 5), "apart {apart}");
        assert!(!payloads.is_empty(), "apart {apart}");
        for payload in &payloads {
            assert_eq!(payload.lane, Lane::Stream, "delivered once, in order");
            old.receive(payload.lane, &payload.bytes, now)
                .expect("the previous holder applies it");
        }
        while let Some(frame) = old.next_frame() {
            stepped.push(frame);
        }
        let ticks: Vec<u64> = stepped.iter().map(|f| f.tick).collect();
        assert_eq!(
            ticks,
            (0..=5).collect::<Vec<u64>>(),
            "apart {apart}: in order, each once"
        );
        assert_eq!(
            stepped.last().map(|f| f.membership.clone()),
            Some(leave.membership.clone()),
            "apart {apart}: it steps the frame that lists its own leave"
        );

        // The new holder is served from its join's frame on, like any seat.
        now += 20;
        let join = host.poll_frame(now).expect("the next tick is due");
        assert!(
            join.inputs.iter().any(|(seat, _)| *seat == SeatId(2)),
            "apart {apart}"
        );
        let out = host.outgoing(now);
        let to_new: Vec<&Outgoing> = out
            .iter()
            .filter(|(to, o)| *to == SeatId(2) && o.lane == Lane::Datagram)
            .map(|(_, o)| o)
            .collect();
        assert_eq!(to_new.len(), 1, "apart {apart}");
        let (header, bodies) = wire::decode_frames_datagram(&to_new[0].bytes).expect("decodes");
        assert_eq!(
            header.base_tick + bodies.len() as u64,
            join.tick + 1,
            "apart {apart}: the new holder's datagram ends at the newest frame"
        );
        assert!(
            header.base_tick > leave_tick,
            "apart {apart}: and never reaches back before its join"
        );
    }

    // A previous holder that acknowledged its leave frame is owed nothing.
    let config = LockstepConfig::at_rate(50);
    let mut host = LockstepHost::<Game>::new(config.clone(), HostRole::Relay);
    host.add_seat(SeatId(2));
    host.start(0);
    let mut old = LockstepSeat::<Game>::new(config, SeatId(2));
    let mut now = 0u64;
    let round = |host: &mut LockstepHost<Game>, old: &mut LockstepSeat<Game>, now: u64| {
        for o in old.outgoing(now) {
            let _ = host.receive(SeatId(2), o.lane, &o.bytes, now);
        }
        while host.poll_frame(now).is_some() {}
        for (_, o) in host.outgoing(now) {
            old.receive(o.lane, &o.bytes, now).expect("host payload");
        }
        while old.next_frame().is_some() {}
    };
    for _ in 0..4 {
        round(&mut host, &mut old, now);
        now += 20;
    }
    host.remove_seat(SeatId(2));
    for _ in 0..3 {
        round(&mut host, &mut old, now);
        now += 20;
    }
    while host.poll_event().is_some() {}
    host.add_seat(SeatId(2));
    assert!(
        !std::iter::from_fn(|| host.poll_event()).any(|e| matches!(e, HostEvent::SeatHandover { .. })),
        "nothing is owed to a holder that acknowledged its leave"
    );
}

/// With an input delay of 0 the input sampled at capture tick 0 is meant for tick 0. A seat added
/// before the start has it in frame 0 when it reached the host before that frame was built, and a
/// listen host's own input that was answered `Tick(0)` is in frame 0 (both used to be dropped:
/// the host buffered nothing for a seat until its join was applied by the first frame).
#[test]
fn with_no_input_delay_the_tick_zero_inputs_of_seats_added_before_the_start_are_in_frame_zero() {
    let config = LockstepConfig {
        initial_delay_ticks: 0,
        min_delay_ticks: 0,
        ..config()
    };
    let mut host = LockstepHost::<Game>::new(config.clone(), HostRole::Listen(SeatId(0)));
    host.add_seat(SeatId(0));
    host.add_seat(SeatId(1));
    let mut seat = LockstepSeat::<Game>::new(config.clone(), SeatId(1));
    let (own, theirs) = (steady_input(SeatId(0), 0), steady_input(SeatId(1), 0));
    assert_eq!(seat.submit_input(&theirs), Ok(Stamp::Tick(0)));
    host.start(0);
    assert_eq!(host.local_input(&own, 0), Ok(Stamp::Tick(0)));
    // The seat's first datagram arrives after the start and before the host's first frame.
    for out in seat.outgoing(3) {
        host.receive(SeatId(1), out.lane, &out.bytes, 3)
            .expect("the seat's inputs");
    }
    let frame = host.poll_frame(3).expect("tick 0 is due");
    assert_eq!(frame.tick, 0);
    assert_eq!(
        frame.inputs,
        vec![
            (SeatId(0), SeatInput::Present(own.clone())),
            (SeatId(1), SeatInput::Present(theirs))
        ],
        "both inputs stamped for tick 0 are in frame 0"
    );
    let status = host.status(3);
    assert!(
        status.seats.iter().all(|s| s.inputs_missing == 0),
        "{:?}",
        status.seats
    );

    // A listen host whose own seat was never added is told so, not answered with a tick whose
    // frame will not carry the input.
    let mut host = LockstepHost::<Game>::new(config, HostRole::Listen(SeatId(0)));
    host.add_seat(SeatId(1));
    host.start(0);
    assert_eq!(
        host.local_input(&own, 0),
        Err(LockstepError::UnknownSeat(SeatId(0)))
    );
}

/// An application that stopped stepping has no hole to repair: the frames it does not take stay
/// unacknowledged at the host. It must not report one (the host answered with every
/// unacknowledged frame in every datagram, a budget-sized datagram per tick, until it marked the
/// seat lagging), and once it steps again it reports the real hole and catches up.
#[test]
fn a_seat_whose_application_stopped_stepping_reports_no_hole_and_is_sent_no_repair() {
    let config = LockstepConfig {
        frame_log_ticks: 64,
        ..config()
    };
    let mut host = LockstepHost::<Game>::new(config.clone(), HostRole::Relay);
    let mut seat = LockstepSeat::<Game>::new(config, SeatId(1));
    host.add_seat(SeatId(1));
    host.start(0);
    let mut stepped = Vec::new();
    let mut frames_in_a_datagram_while_full = 0usize;
    let mut holes_reported_while_full = 0u32;
    let mut repairs_at_the_host = 0u64;
    for now in (0..2_600u64).step_by(16) {
        // The application steps for the first 100 ms, stops for 1.4 s, then steps again.
        let stalled = (100..1_500).contains(&now);
        let full = seat.frames_waiting() >= 64;
        for out in seat.outgoing(now) {
            if out.lane == Lane::Datagram && full {
                let msg = wire::decode_inputs(&out.bytes, 1024).expect("decodes");
                holes_reported_while_full += u32::from(msg.hole);
            }
            host.receive(SeatId(1), out.lane, &out.bytes, now)
                .expect("seat payload");
        }
        while host.poll_frame(now).is_some() {}
        for (_, out) in host.outgoing(now) {
            if out.lane == Lane::Datagram && full && stalled {
                let (_, bodies) = wire::decode_frames_datagram(&out.bytes).expect("decodes");
                frames_in_a_datagram_while_full = frames_in_a_datagram_while_full.max(bodies.len());
            }
            seat.receive(out.lane, &out.bytes, now).expect("host payload");
        }
        if !stalled {
            while let Some(frame) = seat.next_frame() {
                stepped.push(frame.tick);
            }
        }
        if now == 1_488 {
            assert!(
                seat.frames_waiting() >= 64,
                "the seat's queue filled while it stalled"
            );
            repairs_at_the_host = host.status(now).seats[0].hole_repairs;
        }
    }
    assert_eq!(
        holes_reported_while_full, 0,
        "a full seat has no hole: its frames wait in datagrams it does not take"
    );
    assert!(
        frames_in_a_datagram_while_full <= 3,
        "the host kept to its redundancy window: {frames_in_a_datagram_while_full} frames in one datagram"
    );
    assert_eq!(
        repairs_at_the_host, 0,
        "no repair was started for the stalled seat"
    );
    // It steps again: the real hole is reported, repaired, and the seat is back at the newest frame.
    let built = host.tick();
    assert!(
        stepped.iter().copied().eq(0..stepped.len() as u64),
        "in order, each once"
    );
    assert!(
        stepped.len() as u64 + 2 >= built,
        "caught up: stepped {} of {built}",
        stepped.len()
    );
    let status = host.status(2_600);
    assert!(
        !status.seats[0].lagging && status.seats[0].hole_repairs >= 1,
        "{:?}",
        status.seats[0]
    );
}

/// The host ends a repair when the seat stops reporting the hole, not only when the seat has
/// acknowledged the frame the repair was started at.
#[test]
fn a_repair_ends_when_the_seat_no_longer_reports_the_hole() {
    let mut host = LockstepHost::<Game>::new(config(), HostRole::Relay);
    host.add_seat(SeatId(1));
    host.start(0);
    let mut now = 0u64;
    for _ in 0..30 {
        while host.poll_frame(now).is_some() {}
        host.outgoing(now);
        now += 17;
    }
    let report = |seq: u64, hole: bool| {
        wire::encode_inputs(&wire::InputsMsg {
            seq,
            ack_next: 10,
            down_p99_run: 0,
            down_window_run: 0,
            lead: 0,
            hole,
            leave: false,
            catching_up: false,
            votes: Vec::new(),
            first_tick: 10,
            inputs: Vec::new(),
        })
    };
    let frames_sent = |host: &mut LockstepHost<Game>, now: u64| -> usize {
        while host.poll_frame(now).is_some() {}
        let out: Vec<Outgoing> = host.outgoing(now).into_iter().map(|(_, o)| o).collect();
        let (_, bodies) = wire::decode_frames_datagram(&only_datagram(&out).bytes).expect("decodes");
        bodies.len()
    };
    host.receive(SeatId(1), Lane::Datagram, &report(0, true), now)
        .expect("the hole report");
    now += 17;
    assert!(
        frames_sent(&mut host, now) >= 20,
        "the repair sends every unacknowledged frame"
    );
    // The seat stopped reporting the hole (its application stalled): the same acknowledgement.
    host.receive(SeatId(1), Lane::Datagram, &report(1, false), now)
        .expect("no hole any more");
    now += 17;
    assert!(
        frames_sent(&mut host, now) <= 3,
        "the repair ended with the report: back to the redundancy window"
    );
    // A reordered older datagram that still carries the report does not start it again.
    host.receive(SeatId(1), Lane::Datagram, &report(0, true), now)
        .expect("a duplicate");
    now += 17;
    assert!(frames_sent(&mut host, now) <= 3);
}

// ---- late stamping (ADR-8209) --------------------------------------------------------------------

fn late_stamp_events(events: &[HostEvent]) -> Vec<(SeatId, bool)> {
    events
        .iter()
        .filter_map(|e| match e {
            HostEvent::LateStamp { seat, suspect, .. } => Some((*seat, *suspect)),
            _ => None,
        })
        .collect()
}

/// A client that acknowledges frames as they come and sends each input only two ticks before
/// its frame is built, on a link that would deliver it four ticks earlier: the host flags that
/// seat and only that seat, reports it in the status, and removes nobody.
#[test]
fn a_seat_that_holds_its_inputs_back_is_flagged_and_nobody_is_removed() {
    let config = LockstepConfig {
        initial_delay_ticks: 6,
        min_delay_ticks: 6,
        ..config()
    };
    let tick_us = config.tick_us();
    let mut rig = Rig::new(config, HostRole::Relay, &[1, 2, 3], LinkSpec::latency(10));
    rig.seats.get_mut(&SeatId(2)).expect("seat 2").hold_inputs = Some(2);
    rig.run_ms(15_000);
    assert_eq!(
        late_stamp_events(&rig.host_events),
        vec![(SeatId(2), true)],
        "one flag, for the seat that waits"
    );
    let Some(HostEvent::LateStamp {
        hold_us, margin_us, ..
    }) = rig
        .host_events
        .iter()
        .find(|e| matches!(e, HostEvent::LateStamp { .. }))
        .cloned()
    else {
        panic!("no late-stamp event");
    };
    assert!(
        hold_us >= 3 * tick_us && margin_us <= hold_us as i64,
        "held {hold_us} us with {margin_us} us of margin"
    );
    let status = rig.host.status(rig.now);
    assert_eq!(status.late_stamp_suspects, vec![2]);
    for row in &status.seats {
        assert_eq!(row.late_stamp_suspect, row.seat == 2, "{row:?}");
        assert_eq!(row.late_stamp_flags, u32::from(row.seat == 2));
        if row.seat != 2 {
            assert_eq!(
                row.input_hold_p10_us,
                Some(0),
                "an honest seat has no hold: {row:?}"
            );
        }
        // Its inputs still made their frames: this is not a seat on a slow link.
        assert!(row.inputs_missing <= 2, "{row:?}");
    }
    // A flag, not a penalty: the seat is still a member, nothing was ordered, and the match is
    // in step.
    assert_eq!(rig.host.members().count(), 3);
    assert!(
        !rig.host_events
            .iter()
            .any(|e| matches!(e, HostEvent::SeatRemoved { .. } | HostEvent::ResyncSeat { .. })),
        "nobody is removed or resynced for it"
    );
    assert_eq!(rig.host.delay_ticks(), 6, "the delay did not move");
    rig.assert_in_step();

    // It plays straight again: the flag clears once the window holds only honest inputs.
    rig.seats.get_mut(&SeatId(2)).expect("seat 2").hold_inputs = None;
    rig.run_ms(12_000);
    assert_eq!(
        late_stamp_events(&rig.host_events),
        vec![(SeatId(2), true), (SeatId(2), false)]
    );
    assert!(rig.host.status(rig.now).late_stamp_suspects.is_empty());
}

/// Honest seats are never flagged: not on a link whose latency, jitter, loss and reordering put
/// their inputs at the edge of the deadline (they did not wait: the input comes with the
/// acknowledgement), and not when the application samples one frame after it stepped.
#[test]
fn honest_seats_on_a_bad_link_or_with_a_frame_of_pipeline_latency_are_not_flagged() {
    let mut rig = Rig::new(config(), HostRole::Relay, &[1, 2, 3, 4], LinkSpec::BAD_WIFI);
    rig.run_ms(30_000);
    assert_eq!(late_stamp_events(&rig.host_events), vec![], "bad-wifi");
    let status = rig.host.status(rig.now);
    for row in &status.seats {
        assert!(!row.late_stamp_suspect, "{row:?}");
        assert!(
            row.input_hold_p10_us.is_none_or(|hold| hold == 0),
            "an honest seat's input comes with its acknowledgement: {row:?}"
        );
    }
    rig.assert_in_step();

    let config = LockstepConfig {
        initial_delay_ticks: 6,
        min_delay_ticks: 6,
        ..config()
    };
    let tick_us = config.tick_us();
    let mut rig = Rig::new(config, HostRole::Relay, &[1, 2], LinkSpec::latency(10));
    // Released one frame after the capture opened: a hold of one tick.
    rig.seats.get_mut(&SeatId(2)).expect("seat 2").hold_inputs = Some(5);
    rig.run_ms(15_000);
    assert_eq!(
        late_stamp_events(&rig.host_events),
        vec![],
        "one frame of pipeline latency"
    );
    let status = rig.host.status(rig.now);
    let row = status.seats.iter().find(|s| s.seat == 2).expect("seat 2");
    assert!(
        row.input_hold_p10_us
            .is_some_and(|hold| hold > 0 && hold < 2 * tick_us),
        "{row:?}"
    );
}

// ---- interleaved redundancy (ADR-8209) -----------------------------------------------------------

/// What one seat's frames datagrams carry, tick by tick, when the seat acknowledges `lag` ticks
/// late and asks its window to cover a loss run of `run`: `(first frame, frames)` per datagram.
fn datagram_shapes(
    redundancy: FrameRedundancy,
    run: u32,
    lag: u64,
    hole_at: Option<u64>,
) -> Vec<(u64, usize)> {
    let config = LockstepConfig {
        redundancy,
        ..config()
    };
    let mut host = LockstepHost::<Game>::new(config, HostRole::Relay);
    host.add_seat(SeatId(1));
    host.start(0);
    let mut shapes = Vec::new();
    for step in 0..90u64 {
        let now = step * 17;
        let report = wire::encode_inputs(&wire::InputsMsg {
            seq: step,
            ack_next: host.tick().saturating_sub(lag),
            down_p99_run: 0,
            down_window_run: run,
            lead: 0,
            hole: hole_at == Some(step),
            leave: false,
            catching_up: false,
            votes: Vec::new(),
            first_tick: host.tick(),
            inputs: Vec::new(),
        });
        host.receive(SeatId(1), Lane::Datagram, &report, now)
            .expect("the seat's report");
        while host.poll_frame(now).is_some() {}
        let out: Vec<Outgoing> = host.outgoing(now).into_iter().map(|(_, o)| o).collect();
        let (header, bodies) = wire::decode_frames_datagram(&only_datagram(&out).bytes).expect("decodes");
        assert_eq!(
            header.base_tick + bodies.len() as u64,
            host.tick(),
            "every datagram ends at the newest frame"
        );
        shapes.push((header.base_tick, bodies.len()));
    }
    shapes
}

/// The interleaved schedule: two frames in every datagram, and every second datagram the link's
/// loss run plus two. A link that has shown no run of two gets two frames always; `Adaptive`
/// sends the run plus one every time; a hole report gets every unacknowledged frame.
#[test]
fn interleaved_redundancy_sends_two_frames_and_every_second_datagram_the_loss_run_plus_two() {
    let settled = |shapes: &[(u64, usize)]| -> Vec<usize> { shapes[20..40].iter().map(|s| s.1).collect() };
    let lossy = settled(&datagram_shapes(FrameRedundancy::Interleaved, 5, 12, None));
    assert!(
        lossy.chunks(2).all(|pair| {
            let (a, b) = (pair[0], pair[1]);
            (a, b) == (2, 7) || (a, b) == (7, 2)
        }),
        "{lossy:?}"
    );
    let single = settled(&datagram_shapes(FrameRedundancy::Interleaved, 1, 12, None));
    assert!(
        single.iter().all(|n| *n == 2),
        "single losses need no deeper datagram: {single:?}"
    );
    let clean = settled(&datagram_shapes(FrameRedundancy::Interleaved, 0, 12, None));
    assert!(clean.iter().all(|n| *n == 2), "{clean:?}");
    let adaptive = settled(&datagram_shapes(FrameRedundancy::Adaptive, 5, 12, None));
    assert!(
        adaptive.iter().all(|n| *n == 6),
        "the run plus one, every time: {adaptive:?}"
    );
    let all = settled(&datagram_shapes(FrameRedundancy::AllUnacked, 5, 12, None));
    assert!(
        all.iter().all(|n| *n == 12 || *n == 13),
        "every unacknowledged frame: {all:?}"
    );
    // Half the redundancy of `Adaptive` on this link, a sixth of `AllUnacked`.
    assert_eq!(lossy.iter().sum::<usize>() * 2, (2 + 7) * 20);
    // The deeper datagram never carries more than is unacknowledged, and never more than
    // sixteen frames: a run past that is repaired through the hole report.
    let short = settled(&datagram_shapes(FrameRedundancy::Interleaved, 30, 12, None));
    assert_eq!(
        short.iter().max().copied(),
        Some(13),
        "what is unacknowledged: {short:?}"
    );
    let wild = datagram_shapes(FrameRedundancy::Interleaved, 30, 40, None);
    let wild: Vec<usize> = wild[60..80].iter().map(|s| s.1).collect();
    assert_eq!(wild.iter().max().copied(), Some(16), "{wild:?}");
    assert_eq!(wild.iter().min().copied(), Some(2), "{wild:?}");
    // The seat reports a hole at step 30: the next datagram carries every unacknowledged frame.
    let repaired = datagram_shapes(FrameRedundancy::Interleaved, 5, 12, Some(30));
    assert!(repaired[30].1 >= 12, "{:?}", &repaired[28..33]);
}

/// A relay host and one seat at 50 Hz whose payloads take `UP_DELAY_TICKS` ticks to reach the
/// host, while the host's reach the seat at once (or are lost).
struct SlowUp {
    host: LockstepHost<Game>,
    seat: LockstepSeat<Game>,
    up: std::collections::VecDeque<Vec<Outgoing>>,
    stepped: u64,
    now: u64,
}

const UP_DELAY_TICKS: usize = 6;

impl SlowUp {
    fn new() -> Self {
        let config = LockstepConfig::at_rate(50);
        let mut host = LockstepHost::<Game>::new(config.clone(), HostRole::Relay);
        host.add_seat(SeatId(1));
        host.start(0);
        Self {
            host,
            seat: LockstepSeat::new(config, SeatId(1)),
            up: std::collections::VecDeque::new(),
            stepped: 0,
            now: 0,
        }
    }

    /// One tick; with `lose` the host's datagram never arrives. Returns how many frames the
    /// seat is behind the newest one after it.
    fn tick(&mut self, lose: bool) -> u64 {
        let _ = self.seat.submit_input(&steady_input(SeatId(1), self.stepped));
        self.up.push_back(self.seat.outgoing(self.now));
        if self.up.len() > UP_DELAY_TICKS {
            for o in self.up.pop_front().unwrap_or_default() {
                self.host
                    .receive(SeatId(1), o.lane, &o.bytes, self.now)
                    .expect("seat payload");
            }
        }
        while self.host.poll_frame(self.now).is_some() {}
        for (_, o) in self.host.outgoing(self.now) {
            assert_eq!(o.lane, Lane::Datagram, "nothing needs the stream");
            if !lose {
                self.seat
                    .receive(o.lane, &o.bytes, self.now)
                    .expect("host payload");
            }
        }
        while let Some(frame) = self.seat.next_frame() {
            assert_eq!(frame.tick, self.stepped, "in order, each once");
            self.stepped += 1;
        }
        self.now += 20;
        self.host.tick() - self.stepped
    }

    fn hole_repairs(&mut self) -> u64 {
        self.host.status(self.now).seats[0].hole_repairs
    }
}

/// The guarantee behind the schedule, with a real seat whose reports take six ticks to reach
/// the host (so nothing here is repaired by a hole report): a single lost datagram is repaired
/// by the next one, and a run of two to five by the first or the second datagram after it,
/// whichever of the two the run ends before.
#[test]
fn under_interleaved_redundancy_a_loss_run_is_repaired_within_two_datagrams_without_a_report() {
    for (run, shift) in [
        (1u64, 0u64),
        (1, 1),
        (2, 0),
        (2, 1),
        (3, 0),
        (3, 1),
        (5, 0),
        (5, 1),
    ] {
        let mut link = SlowUp::new();
        // The link shows a run of five once (repaired through the report), then settles.
        for i in 0..100 {
            link.tick((20..25).contains(&i));
        }
        assert_eq!(link.tick(false), 0, "settled");
        let repairs_before = link.hole_repairs();
        for _ in 0..shift {
            link.tick(false);
        }
        for _ in 0..run {
            link.tick(true);
        }
        let after_one = link.tick(false);
        let after_two = link.tick(false);
        if run == 1 {
            assert_eq!(
                after_one, 0,
                "shift {shift}: one lost datagram is repaired by the next"
            );
        }
        assert_eq!(
            after_two, 0,
            "run {run}, shift {shift}: repaired by the second datagram after it at the latest \
             (behind by {after_one} after the first)"
        );
        assert_eq!(
            link.hole_repairs(),
            repairs_before,
            "run {run}, shift {shift}: the schedule repaired it, not a report"
        );
    }
}

/// A deeper datagram whose frames do not fit the budget drops its oldest repeats; it does not
/// push them onto the stream (they were sent before, and the stream is for frames no datagram
/// can carry). `Adaptive` keeps the first pass's rule: what does not fit goes on the stream.
#[test]
fn an_interleaved_window_that_does_not_fit_drops_repeats_and_keeps_off_the_stream() {
    let shapes = |redundancy: FrameRedundancy| -> (Vec<usize>, usize, usize) {
        let config = LockstepConfig {
            redundancy,
            ..config()
        };
        let budget = config.datagram_budget_bytes;
        let mut host = LockstepHost::<Game>::new(config, HostRole::Relay);
        host.add_seat(SeatId(1));
        host.start(0);
        let (mut frames, mut stream_payloads, mut largest) = (Vec::new(), 0usize, 0usize);
        for step in 0..60u64 {
            let now = step * 17;
            // Every tick carries a new 200-byte input, so a frame costs over 200 bytes: four fit.
            let input = Pad {
                blob: (0..200u64).map(|i| (i * 31 + step * 7) as u8).collect(),
                ..steady_input(SeatId(1), step)
            };
            let report = wire::encode_inputs(&wire::InputsMsg {
                seq: step,
                ack_next: host.tick().saturating_sub(12),
                down_p99_run: 0,
                down_window_run: 5,
                lead: 0,
                hole: false,
                leave: false,
                catching_up: false,
                votes: Vec::new(),
                first_tick: host.tick() + 1,
                inputs: vec![Some(input.to_bytes().expect("encodes"))],
            });
            host.receive(SeatId(1), Lane::Datagram, &report, now)
                .expect("the seat's report");
            while host.poll_frame(now).is_some() {}
            let out: Vec<Outgoing> = host.outgoing(now).into_iter().map(|(_, o)| o).collect();
            stream_payloads += out.iter().filter(|o| o.lane == Lane::Stream).count();
            let datagram = only_datagram(&out);
            largest = largest.max(datagram.bytes.len());
            assert!(datagram.bytes.len() <= budget);
            let (header, bodies) = wire::decode_frames_datagram(&datagram.bytes).expect("decodes");
            assert_eq!(
                header.base_tick + bodies.len() as u64,
                host.tick(),
                "ends at the newest frame"
            );
            if step >= 20 {
                frames.push(bodies.len());
            }
        }
        (frames, stream_payloads, largest)
    };
    let (frames, stream_payloads, largest) = shapes(FrameRedundancy::Interleaved);
    assert_eq!(
        stream_payloads, 0,
        "repeats that do not fit are dropped, not streamed"
    );
    assert!(
        frames
            .chunks(2)
            .all(|pair| pair.contains(&2) && pair.iter().any(|n| (4..=5).contains(n))),
        "two frames, and every second datagram the four or five that fit of the seven it wants: {frames:?}"
    );
    assert!(
        largest > 800,
        "the deeper datagram uses the budget: {largest} bytes"
    );
    let (_, adaptive_stream, _) = shapes(FrameRedundancy::Adaptive);
    assert!(
        adaptive_stream > 0,
        "the first pass's rule: the overflow goes on the stream"
    );
}

/// The same on links that are not clean: 15 ms plus up to 10 ms of jitter each way, 1 % loss
/// and bursts, duplicates. The seat that sits on its inputs is flagged, the honest seats on the
/// same links are not, and nobody is removed.
#[test]
fn on_jittery_lossy_links_the_seat_that_holds_its_inputs_back_is_still_the_only_one_flagged() {
    const ROUGH: LinkSpec = LinkSpec {
        latency_ms: 15,
        jitter_ms: 10,
        loss_per_mille: 10,
        burst_start_per_mille: 5,
        burst_stay_per_mille: 300,
        dup_per_mille: 2,
    };
    let config = LockstepConfig {
        initial_delay_ticks: 9,
        min_delay_ticks: 9,
        ..config()
    };
    let tick_us = config.tick_us();
    let mut rig = Rig::new(config, HostRole::Relay, &[1, 2, 3, 4], ROUGH);
    // Released when the seat is four ticks from stepping the stamped tick: five ticks after an
    // honest seat would have sent it, and still in time on this link.
    rig.seats.get_mut(&SeatId(3)).expect("seat 3").hold_inputs = Some(4);
    rig.run_ms(40_000);
    let events = late_stamp_events(&rig.host_events);
    assert!(
        events.contains(&(SeatId(3), true)),
        "the seat that waits is flagged: {events:?}"
    );
    assert!(
        events.iter().all(|(seat, _)| *seat == SeatId(3)),
        "no honest seat is flagged on the same links: {events:?}"
    );
    let status = rig.host.status(rig.now);
    assert_eq!(status.late_stamp_suspects, vec![3], "{:?}", status.seats);
    for row in &status.seats {
        if row.seat == 3 {
            assert!(
                row.input_hold_p10_us.is_some_and(|hold| hold >= 3 * tick_us),
                "{row:?}"
            );
        } else {
            assert_eq!(row.input_hold_p10_us, Some(0), "{row:?}");
            assert_eq!(row.late_stamp_flags, 0, "{row:?}");
        }
    }
    assert_eq!(rig.host.members().count(), 4);
    assert!(
        !rig.host_events
            .iter()
            .any(|e| matches!(e, HostEvent::SeatRemoved { .. })),
        "a flag, not a penalty"
    );
    rig.assert_in_step();
}

/// A seat's reported downlink loss run is not a measurement the host made. Under
/// [`DelayPolicy::Shared`] the largest need becomes every seat's input delay, and a raise
/// applies at once. One datagram that carries a real first-copy input and `down_p99_run = 32`
/// (`loss::MAX_RUN`) must leave the match on its initial delay.
///
/// Review proof for lockstep-server-host at 14dda69d0 (ADR-8558). The host stores the reported
/// run in `SeatTiming::down_p99_run` and `DelayControl::target` takes the max, so this fails
/// while that report is trusted: the shared delay moves from 4 to `max_delay_ticks` (15).
#[test]
fn a_reported_downlink_loss_run_does_not_raise_the_shared_delay() {
    let mut host = LockstepHost::<Game>::new(config(), HostRole::Relay);
    host.add_seat(SeatId(0));
    host.add_seat(SeatId(1));
    host.start(0);
    let pad = Pad::default().to_bytes().expect("encodes");
    let mut now = 0u64;
    for _ in 0..8 {
        now += 17;
        while host.poll_frame(now).is_some() {}
    }
    let tick = host.tick();
    let datagram = |down_p99_run: u32| {
        wire::encode_inputs(&wire::InputsMsg {
            seq: 0,
            ack_next: tick,
            down_p99_run,
            down_window_run: 0,
            lead: 0,
            hole: false,
            leave: false,
            catching_up: false,
            votes: Vec::new(),
            first_tick: tick,
            inputs: vec![Some(pad.clone())],
        })
    };
    host.receive(SeatId(0), Lane::Datagram, &datagram(0), now)
        .expect("honest seat");
    host.receive(SeatId(1), Lane::Datagram, &datagram(32), now)
        .expect("reporting seat");
    let initial = host.config().initial_delay_ticks;
    // The join lands on the next frame. The delay check runs at least every tick_rate/4
    // ticks. 60 steps of 17 ms stay inside the 2 s sample window.
    for _ in 0..60 {
        now += 17;
        while host.poll_frame(now).is_some() {}
    }
    assert!(
        host.delay_ticks() <= initial,
        "seat 1 reported down_p99_run 32; shared delay moved from {initial} to {}",
        host.delay_ticks()
    );
    assert!(host.tick() > tick, "frames kept moving");
    assert_eq!(
        host.members().collect::<Vec<_>>(),
        vec![SeatId(0), SeatId(1)],
        "both seats stayed"
    );
}

/// Seats that send one crafted inputs datagram per step for `ms` of virtual time, each carrying a
/// real first-copy input for the next tick and the `(down_p99_run, lead)` that seat reports.
fn reported_grief(policy: DelayPolicy, reports: &[(u32, u16)], ms: u64) -> LockstepHost<Game> {
    let mut host = LockstepHost::<Game>::new(
        LockstepConfig {
            delay_policy: policy,
            ..config()
        },
        HostRole::Relay,
    );
    for i in 0..reports.len() {
        host.add_seat(SeatId(i as u16));
    }
    host.start(0);
    let pad = Pad::default().to_bytes().expect("encodes");
    let mut now = 0u64;
    let mut seq = 0u64;
    while now < ms {
        now += 17;
        while host.poll_frame(now).is_some() {}
        let tick = host.tick();
        for (i, &(down_p99_run, lead)) in reports.iter().enumerate() {
            let bytes = wire::encode_inputs(&wire::InputsMsg {
                seq,
                ack_next: tick,
                down_p99_run,
                down_window_run: 0,
                lead,
                hole: false,
                leave: false,
                catching_up: false,
                votes: Vec::new(),
                first_tick: tick,
                inputs: vec![Some(pad.clone())],
            });
            host.receive(SeatId(i as u16), Lane::Datagram, &bytes, now)
                .expect("seat bytes");
        }
        seq = seq.wrapping_add(1);
    }
    host
}

/// ADR-8656: a reported lead is a seat's own stamping offset, which only `PerSeat` lets a seat
/// run. Under `Shared` it would add to the seat's measured lateness (the term that raises every
/// seat's delay), so the host takes none: twenty seconds of `lead = 8` from one seat of two, on
/// a perfect link, leave the match on its initial delay and the seat's lead row at 0.
#[test]
fn a_reported_lead_does_not_raise_the_shared_delay() {
    let mut host = reported_grief(DelayPolicy::Shared, &[(0, 0), (0, 8)], 20_000);
    let initial = host.config().initial_delay_ticks;
    assert!(
        host.delay_ticks() <= initial,
        "seat 1 reported lead 8; shared delay moved from {initial} to {}",
        host.delay_ticks()
    );
    let status = host.status(20_000);
    assert!(
        status.seats.iter().all(|s| s.extra_lead_ticks == 0),
        "{:?}",
        status.seats
    );
    assert_eq!(status.delay_raises, 0);
    assert_eq!(
        host.members().count(),
        2,
        "a reporter is not removed for its report"
    );
}

/// ADR-8656: a sustained report (every datagram for twenty seconds, both terms at their caps)
/// does not raise the shared delay either; the seat's reported run still shows in its status row.
#[test]
fn a_sustained_downlink_and_lead_report_does_not_raise_the_shared_delay() {
    let mut host = reported_grief(DelayPolicy::Shared, &[(0, 0), (32, 8), (0, 0)], 20_000);
    let initial = host.config().initial_delay_ticks;
    assert!(host.delay_ticks() <= initial, "delay {}", host.delay_ticks());
    let status = host.status(20_000);
    assert_eq!(status.delay_raises, 0);
    let griefer = status.seats.iter().find(|s| s.seat == 1).expect("seat 1");
    assert_eq!(griefer.loss_run_p99, 32, "the report is shown, not used");
    assert!(
        griefer.delay_need_ticks.is_some_and(|n| n <= initial),
        "{griefer:?}"
    );
    assert!(host.tick() > 1_000, "frames kept moving: {}", host.tick());
}

/// ADR-8656: under `PerSeat` the reports count, but the frame delay follows the lower median
/// need: one reporting seat of three does not move it; two of three (a majority) do. That is the
/// bound the policy gives, and it is what the test pins.
#[test]
fn per_seat_delay_follows_the_median_not_one_reporting_seat() {
    let host = reported_grief(DelayPolicy::PerSeat, &[(0, 0), (0, 0), (32, 8)], 10_000);
    let initial = host.config().initial_delay_ticks;
    assert!(
        host.delay_ticks() <= initial,
        "one of three moved it to {}",
        host.delay_ticks()
    );
    let host = reported_grief(DelayPolicy::PerSeat, &[(0, 0), (32, 8), (32, 8)], 10_000);
    assert!(host.delay_ticks() > initial, "a majority of needs is the median");
}

mod rollback;
mod sessions;
