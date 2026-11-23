// SPDX-License-Identifier: MPL-2.0
//! The successor journal on a scripted pipe (spec 120 § 10.5, ADR-8853): seeded, tick-stepped,
//! no sockets and no clock. The acceptance runs on impaired links are in
//! `crates/regia_net_soak/tests/migration_journal.rs`.

use std::collections::{BTreeMap, VecDeque};

use super::*;

const CELLS: usize = 8;

/// A small deterministic world: eight cells the inputs and events of a tick stir, and a blob
/// that makes the full state as large as a test wants.
#[derive(Debug, Clone, Default)]
struct ToyWorld {
    tick: u64,
    cells: [u64; CELLS],
    blob: Vec<u8>,
    /// The digest after each applied tick (the shadow's own record).
    applied: BTreeMap<u64, WorldDigest>,
    /// The digest after each loaded full state.
    loaded: BTreeMap<u64, WorldDigest>,
}

impl ToyWorld {
    fn with_blob(len: usize) -> Self {
        Self {
            blob: (0..len).map(|i| (i * 31 % 251) as u8).collect(),
            ..Self::default()
        }
    }

    fn step(&mut self, tick: u64, inputs: &[u8], events: &[u8]) {
        self.tick = tick;
        for (i, b) in inputs.iter().enumerate() {
            let c = &mut self.cells[i % CELLS];
            *c = c.rotate_left(5) ^ u64::from(*b).wrapping_add(tick);
        }
        for e in events {
            let c = &mut self.cells[usize::from(*e) % CELLS];
            *c = c.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(u64::from(*e));
        }
        if !self.blob.is_empty() {
            let at = (tick % self.blob.len() as u64) as usize;
            self.blob[at] ^= inputs.first().copied().unwrap_or(1);
        }
    }

    fn encode(&self) -> Vec<u8> {
        let mut w = WireWriter::new();
        w.u64(self.tick);
        for c in self.cells {
            w.u64(c);
        }
        w.u32(self.blob.len() as u32);
        w.raw(&self.blob);
        w.finish()
    }

    fn world_digest(&self) -> WorldDigest {
        *blake3::hash(&self.encode()).as_bytes()
    }
}

impl ShadowWorld for ToyWorld {
    fn load_full(&mut self, tick: u64, bytes: &[u8]) -> Result<(), String> {
        let mut r = WireReader::new(bytes);
        let read = |r: &mut WireReader<'_>| -> Result<(u64, [u64; CELLS], Vec<u8>), WireError> {
            let t = r.u64()?;
            let mut cells = [0; CELLS];
            for c in &mut cells {
                *c = r.u64()?;
            }
            let len = r.u32()? as usize;
            let blob = r.take(len)?.to_vec();
            r.finish()?;
            Ok((t, cells, blob))
        };
        let (t, cells, blob) = read(&mut r).map_err(|e| e.to_string())?;
        if t != tick {
            return Err(format!("full state of tick {t} sent as tick {tick}"));
        }
        self.tick = t;
        self.cells = cells;
        self.blob = blob;
        self.loaded.insert(tick, self.world_digest());
        Ok(())
    }

    fn apply(&mut self, tick: u64, inputs: &[u8], events: &[u8]) -> Result<(), String> {
        if tick != self.tick + 1 {
            return Err(format!("tick {tick} applied on a world at {}", self.tick));
        }
        self.step(tick, inputs, events);
        self.applied.insert(tick, self.world_digest());
        Ok(())
    }

    fn digest(&self) -> WorldDigest {
        self.world_digest()
    }
}

fn rng(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

fn candidate(peer: u32, rtt_ms: f32) -> Candidate {
    Candidate {
        peer: PeerId(peer),
        rtt_ms,
        jitter_ms: 1.0,
        loss_fraction: 0.0,
        interrupted: false,
        allowance_bytes_per_s: None,
    }
}

/// A host, its world and every client's successor, joined by an ordered pipe that delivers a
/// payload `delay_ticks` host ticks after it was handed out (acknowledgments take as long back).
struct Rig {
    host: JournalHost,
    world: ToyWorld,
    shadows: BTreeMap<u32, JournalSuccessor<ToyWorld>>,
    candidates: Vec<Candidate>,
    tick: u64,
    now_ms: u64,
    allowance: Option<u32>,
    /// Allowances of single peers, over `allowance`.
    allowance_of: BTreeMap<u32, u32>,
    delay_ticks: u64,
    take_fulls: bool,
    /// Events the host applies and leaves out of the journal (a game bug).
    forget_events: bool,
    /// Full states to journal with a wrong digest (a game bug).
    wrong_digests: u32,
    full_digests: BTreeMap<u64, WorldDigest>,
    in_flight: VecDeque<(u64, JournalOut)>,
    acks: VecDeque<(u64, PeerId, Vec<u8>)>,
    refusals: Vec<(u32, &'static str)>,
    /// Bytes handed out per peer per whole second.
    sent: BTreeMap<(u32, u64), u64>,
    /// Bytes handed out by the last poll.
    last_handed: u64,
    events: Vec<JournalEvent>,
    seed: u64,
}

impl Rig {
    fn new(config: JournalConfig, peers: &[(u32, f32)], blob: usize) -> Self {
        Self {
            host: JournalHost::new(config),
            world: ToyWorld::with_blob(blob),
            shadows: peers
                .iter()
                .map(|(p, _)| (*p, JournalSuccessor::new(ToyWorld::default())))
                .collect(),
            candidates: peers.iter().map(|(p, rtt)| candidate(*p, *rtt)).collect(),
            tick: 0,
            now_ms: 0,
            allowance: Some(4 * 1024 * 1024),
            allowance_of: BTreeMap::new(),
            delay_ticks: 0,
            take_fulls: true,
            forget_events: false,
            wrong_digests: 0,
            full_digests: BTreeMap::new(),
            in_flight: VecDeque::new(),
            acks: VecDeque::new(),
            refusals: Vec::new(),
            sent: BTreeMap::new(),
            last_handed: 0,
            events: Vec::new(),
            seed: 0x5EED_0001,
        }
    }

    fn remove(&mut self, peer: u32) {
        self.candidates.retain(|c| c.peer.0 != peer);
        self.shadows.remove(&peer);
    }

    fn step(&mut self) {
        self.tick += 1;
        self.now_ms = self.tick * 1_000 / 60;
        let r = rng(&mut self.seed);
        let inputs = r.to_le_bytes();
        let events: Vec<u8> = if r.is_multiple_of(7) {
            vec![(r >> 8) as u8, (r >> 16) as u8]
        } else {
            Vec::new()
        };
        self.world.step(self.tick, &inputs, &events);
        let journaled: &[u8] = if self.forget_events { &[] } else { &events };
        if self.take_fulls && self.host.full_wanted(self.now_ms) {
            let digest = self.world.world_digest();
            self.full_digests.insert(self.tick, digest);
            let sent = if self.wrong_digests > 0 {
                self.wrong_digests -= 1;
                [0xEE; DIGEST_BYTES]
            } else {
                digest
            };
            self.host
                .record_tick(self.tick, &inputs, journaled, Some(sent))
                .expect("record_tick");
            self.host
                .record_full(self.now_ms, self.tick, self.world.encode(), sent)
                .expect("record_full");
        } else {
            self.host
                .record_tick(self.tick, &inputs, journaled, None)
                .expect("record_tick");
        }
        self.host.set_candidates(self.now_ms, &self.candidates);
        let allowance = self.allowance;
        let allowance_of = self.allowance_of.clone();
        self.last_handed = 0;
        for out in self.host.poll(self.now_ms, |peer| {
            allowance_of.get(&peer.0).copied().or(allowance)
        }) {
            assert!(out.bytes.len() <= MAX_JOURNAL_PAYLOAD_BYTES);
            self.last_handed += out.bytes.len() as u64;
            *self.sent.entry((out.peer.0, self.now_ms / 1_000)).or_default() += out.bytes.len() as u64;
            self.in_flight.push_back((self.tick + self.delay_ticks, out));
        }
        while self.in_flight.front().is_some_and(|(due, _)| *due <= self.tick) {
            let Some((_, out)) = self.in_flight.pop_front() else {
                break;
            };
            if let Some(shadow) = self.shadows.get_mut(&out.peer.0)
                && let Err(e) = shadow.on_payload(&out.bytes)
            {
                self.refusals.push((out.peer.0, e.code()));
            }
        }
        for (peer, shadow) in &mut self.shadows {
            if let Some(ack) = shadow.poll_ack() {
                self.acks
                    .push_back((self.tick + self.delay_ticks, PeerId(*peer), ack));
            }
        }
        while self.acks.front().is_some_and(|(due, ..)| *due <= self.tick) {
            let Some((_, peer, ack)) = self.acks.pop_front() else {
                break;
            };
            // A successor that left may still have an acknowledgment on the way.
            let _ = self.host.on_payload(peer, &ack);
        }
        while let Some(e) = self.host.poll_event() {
            self.events.push(e);
        }
    }

    fn run(&mut self, ticks: u64) {
        for _ in 0..ticks {
            self.step();
        }
    }

    fn shadow(&self, peer: u32) -> &JournalSuccessor<ToyWorld> {
        self.shadows.get(&peer).expect("shadow")
    }

    /// Every full-state tick the shadow has passed, checked against the host's digest there:
    /// by the replayed log (`applied`) and by the loaded full state (`loaded`). Returns how many
    /// were checked by replay.
    fn assert_shadow_matches_at_full_states(&self, peer: u32) -> usize {
        let shadow = self.shadow(peer);
        let applied = shadow.applied_tick().expect("applied");
        let mut by_replay = 0;
        for (tick, digest) in &self.full_digests {
            if *tick > applied {
                continue;
            }
            if let Some(d) = shadow.world().applied.get(tick) {
                assert_eq!(
                    d, digest,
                    "peer {peer}: replayed shadow differs at full-state tick {tick}"
                );
                by_replay += 1;
            }
            if let Some(d) = shadow.world().loaded.get(tick) {
                assert_eq!(
                    d, digest,
                    "peer {peer}: loaded shadow differs at full-state tick {tick}"
                );
            }
        }
        by_replay
    }
}

/// Every payload of a full state: its start, then its bytes in parts of `part`.
fn full_payloads(appointment: u32, full: &FullState, part: usize) -> Vec<Vec<u8>> {
    let mut out = vec![encode_full_begin(appointment, full, true)];
    let mut offset = 0;
    while offset < full.bytes.len() {
        let len = part.min(full.bytes.len() - offset);
        out.push(encode_full_data(appointment, full, offset, len));
        offset += len;
    }
    out
}

fn config() -> JournalConfig {
    JournalConfig::default()
}

/// Every payload kind encodes and decodes to the same fields.
#[test]
fn the_journal_payloads_round_trip() {
    let Ok(Msg::Appoint {
        appointment,
        rank,
        depth_ticks,
    }) = decode(&encode_appoint(7, 1, 600))
    else {
        panic!("appoint");
    };
    assert_eq!((appointment, rank, depth_ticks), (7, 1, 600));
    assert!(matches!(
        decode(&encode_dismiss(9)),
        Ok(Msg::Dismiss { appointment: 9 })
    ));
    let full = FullState {
        seq: 3,
        tick: 1_234,
        bytes: (0..20_000u32).map(|i| i as u8).collect(),
        digest: [5; DIGEST_BYTES],
    };
    let payloads = full_payloads(11, &full, 8 * 1024);
    assert_eq!(payloads.len(), 4, "the start and three data parts");
    assert_eq!(payloads[0].len(), FULL_BEGIN_BYTES);
    let Ok(Msg::FullBegin {
        appointment,
        seq,
        reseed,
        tick,
        total,
        digest,
    }) = decode(&payloads[0])
    else {
        panic!("full start");
    };
    assert_eq!(
        (appointment, seq, reseed, tick, total, digest),
        (11, 3, true, 1_234, 20_000, [5; DIGEST_BYTES])
    );
    assert!(matches!(
        decode(&encode_full_begin(11, &full, false)),
        Ok(Msg::FullBegin { reseed: false, .. })
    ));
    let mut joined = Vec::new();
    for bytes in &payloads[1..] {
        assert!(is_journal_payload(bytes));
        let Ok(Msg::FullData {
            appointment,
            seq,
            offset,
            bytes: part,
        }) = decode(bytes)
        else {
            panic!("full data");
        };
        assert_eq!((appointment, seq, offset as usize), (11, 3, joined.len()));
        assert_eq!(bytes.len(), FULL_DATA_HEADER_BYTES + part.len());
        joined.extend_from_slice(part);
    }
    assert_eq!(joined, full.bytes);
    let records = [
        JournalRecord {
            tick: 50,
            inputs: vec![1, 2, 3].into(),
            events: Vec::new().into(),
            digest: None,
        },
        JournalRecord {
            tick: 51,
            inputs: Vec::new().into(),
            events: vec![9; 300].into(),
            digest: Some([8; DIGEST_BYTES]),
        },
    ];
    let bytes = encode_log(4, 50, records.iter());
    assert_eq!(
        bytes.len(),
        LOG_HEADER_BYTES + records.iter().map(record_wire_len).sum::<usize>()
    );
    let Ok(Msg::Log {
        appointment_low,
        tick_low,
        records: got,
    }) = decode(&bytes)
    else {
        panic!("log");
    };
    assert_eq!((appointment_low, tick_low, got.len()), (4, 50, 2));
    // An empty tick is six bytes: the five-byte header and one flags byte.
    let empty = JournalRecord {
        tick: 70_000,
        inputs: Vec::new().into(),
        events: Vec::new().into(),
        digest: None,
    };
    let bytes = encode_log(0x0103, 70_000, [empty].iter());
    assert_eq!(bytes.len(), 6);
    assert!(matches!(
        decode(&bytes),
        Ok(Msg::Log {
            appointment_low: 3,
            tick_low: 4_464,
            ..
        })
    ));
    assert_eq!(got[0].inputs, [1, 2, 3]);
    assert_eq!(got[1].events.len(), 300);
    assert_eq!(got[1].digest, Some([8; DIGEST_BYTES]));
    let ack = Ack {
        appointment: 4,
        state: ShadowState::Warm,
        applied_tick: 51,
        seen_seq: 3,
        digest_mismatches: 1,
        replay_divergences: 2,
        refused: 3,
    };
    assert!(matches!(decode(&encode_ack(&ack)), Ok(Msg::Ack(a)) if a == ack));
    assert!(!is_journal_payload(&[0x14, 0, 0]));
    assert!(!is_journal_payload(&[]));
}

/// The host picks its two best connected clients, seeds them, and each shadow has the host's
/// digest at every full-state tick; the shadows adopt later full states without loading them.
#[test]
fn two_warm_successors_match_the_host_at_every_full_state() {
    let mut rig = Rig::new(config(), &[(1, 80.0), (2, 12.0), (3, 30.0), (4, 200.0)], 4_000);
    rig.run(1_520);
    assert_eq!(rig.host.successors(), vec![PeerId(2), PeerId(3)]);
    assert!(rig.refusals.is_empty(), "{:?}", rig.refusals);
    // 25.3 s at a full state every 5 s: the seed (tick 2) and five more.
    assert_eq!(rig.full_digests.len(), 6, "{:?}", rig.full_digests.keys());
    for peer in [2, 3] {
        let checked = rig.assert_shadow_matches_at_full_states(peer);
        assert_eq!(
            checked, 5,
            "peer {peer}: every full state after the seed is checked by replay"
        );
        let shadow = rig.shadow(peer);
        assert_eq!(shadow.applied_tick(), Some(1_520));
        assert_eq!(shadow.world().world_digest(), rig.world.world_digest());
        let status = shadow.status();
        assert_eq!(status.role, "successor");
        assert_eq!(status.state, "warm");
        assert_eq!(status.reloads, 1, "later full states are adopted, not loaded");
        assert_eq!(status.full_states, 6);
        assert_eq!(status.replay_divergences, 0);
        assert_eq!(status.digest_mismatches, 0);
        assert!(status.digest_checks >= 6);
        assert!(status.promotable);
        assert!(
            status.log_ticks <= 300,
            "the log restarts at each full state: {}",
            status.log_ticks
        );
        assert_eq!(status.base_tick, rig.full_digests.keys().last().copied());
        assert!(rig.host.is_warm(PeerId(peer)));
    }
    // The clients that were not picked stay idle.
    assert_eq!(rig.shadow(1).status().role, "client");
    assert_eq!(rig.shadow(1).status().state, "idle");
    let status = rig.host.status();
    assert_eq!(status.role, "host");
    assert_eq!(status.state, "warm");
    assert_eq!(status.tick, Some(1_520));
    assert_eq!(status.successors.len(), 2);
    assert_eq!(status.successors[0].peer, 2);
    assert_eq!(status.successors[0].rank, 0);
    assert_eq!(status.successors[1].rank, 1);
    assert_eq!(status.journal_lag_ticks_p99, 0, "a pipe without delay has no lag");
    assert!(rig.events.contains(&JournalEvent::Warm { peer: PeerId(2) }));
    // Promotion hands over the world at the newest tick, with the journal it came from.
    let host_digest = rig.world.world_digest();
    let promoted = rig
        .shadows
        .remove(&2)
        .expect("shadow")
        .promote()
        .expect("promote");
    assert_eq!(promoted.tick, 1_520);
    assert_eq!(promoted.world.world_digest(), host_digest);
    assert_eq!(promoted.base_tick, *rig.full_digests.keys().last().expect("full"));
    assert_eq!(promoted.log.len() as u64, promoted.tick - promoted.base_tick);
    let mut replayed = ToyWorld::default();
    replayed
        .load_full(promoted.base_tick, &promoted.base)
        .expect("base loads");
    for r in &promoted.log {
        replayed.apply(r.tick, &r.inputs, &r.events).expect("log applies");
    }
    assert_eq!(
        replayed.world_digest(),
        host_digest,
        "the full state plus the log is the world"
    );
}

/// Journal depth edge: a record at exactly the depth after the full state is taken; one past is
/// refused with `journal_past_window`, the shadow is stale and cannot be promoted.
#[test]
fn journal_depth_edge_one_past_the_window_is_refused() {
    let world = ToyWorld {
        tick: 100,
        ..ToyWorld::default()
    };
    let mut shadow = JournalSuccessor::new(ToyWorld::default());
    shadow.on_payload(&encode_appoint(1, 0, 600)).expect("appoint");
    let full = FullState {
        seq: 1,
        tick: 100,
        bytes: world.encode(),
        digest: world.world_digest(),
    };
    for bytes in full_payloads(1, &full, JOURNAL_PART_BYTES) {
        shadow.on_payload(&bytes).expect("full");
    }
    assert!(shadow.promotable());
    let record = |tick: u64| JournalRecord {
        tick,
        inputs: vec![tick as u8].into(),
        events: Vec::new().into(),
        digest: None,
    };
    for tick in 101..=700 {
        shadow
            .on_payload(&encode_log(1, tick, [record(tick)].iter()))
            .unwrap_or_else(|e| panic!("tick {tick} is inside the window: {e}"));
    }
    assert_eq!(
        shadow.applied_tick(),
        Some(700),
        "the record at the depth is applied"
    );
    assert_eq!(shadow.status().log_ticks, 600);
    assert!(shadow.promotable());
    let err = shadow
        .on_payload(&encode_log(1, 701, [record(701)].iter()))
        .expect_err("one past the window");
    assert_eq!(
        err,
        JournalError::PastWindow {
            tick: 701,
            limit: 700
        }
    );
    assert_eq!(err.code(), "journal_past_window");
    let status = shadow.status();
    assert_eq!(status.state, "stale");
    assert_eq!(status.refused, 1);
    assert_eq!(status.last_refusal.as_deref(), Some("journal_past_window"));
    assert!(!status.promotable);
    assert_eq!(status.log_ticks, 0, "a stale shadow holds no journal");
    let ack = shadow.poll_ack().expect("the refusal is acknowledged");
    assert!(matches!(decode(&ack), Ok(Msg::Ack(a)) if a.state == ShadowState::Stale && a.refused == 1));
    assert_eq!(shadow.promote().expect_err("stale").code(), "journal_not_warm");
}

/// The host does not send past the window either. With no new full state its log waits at the
/// depth: the shadows keep what they have and stay promotable, and the next full state carries
/// them to the present. Only when the host's ring no longer holds the next record (two depths)
/// does a successor count as fallen behind and start again from a full state.
#[test]
fn the_host_stops_at_the_window_and_a_new_full_state_resumes() {
    let mut rig = Rig::new(config(), &[(1, 10.0), (2, 20.0)], 0);
    rig.run(10);
    let base = *rig.full_digests.keys().next().expect("seed");
    rig.take_fulls = false;
    rig.run(700);
    assert!(
        rig.refusals.is_empty(),
        "the host sent past the window: {:?}",
        rig.refusals
    );
    for peer in [1, 2] {
        assert_eq!(rig.shadow(peer).applied_tick(), Some(base + 600));
        assert!(rig.shadow(peer).promotable(), "the shadow keeps what it has");
    }
    assert!(rig.host.full_wanted(rig.now_ms));
    let status = rig.host.status();
    for row in &status.successors {
        assert_eq!(row.fell_behind, 0, "{row:?}");
        assert_eq!(row.queued_ticks, rig.tick - (base + 600), "{row:?}");
        assert_eq!(row.lag_ticks, rig.tick - 1 - (base + 600), "{row:?}");
    }
    rig.take_fulls = true;
    rig.run(5);
    for peer in [1, 2] {
        assert_eq!(rig.shadow(peer).applied_tick(), Some(rig.tick));
        assert_eq!(rig.shadow(peer).world().world_digest(), rig.world.world_digest());
        assert_eq!(rig.shadow(peer).status().reloads, 2, "the full state was loaded");
        assert!(rig.host.is_warm(PeerId(peer)));
    }
    assert_eq!(rig.host.status().successors[0].queued_ticks, 0);
    assert!(rig.refusals.is_empty(), "{:?}", rig.refusals);
    // No full state for more than two depths: the ring moves past the next record.
    rig.take_fulls = false;
    rig.run(1_300);
    let status = rig.host.status();
    assert!(status.successors.iter().all(|s| s.fell_behind == 1), "{status:?}");
    assert!(
        status.successors.iter().all(|s| s.state == "seeding"),
        "{status:?}"
    );
    assert_eq!(status.state, "seeding");
    assert!(rig.host.full_wanted(rig.now_ms));
    rig.take_fulls = true;
    rig.run(5);
    for peer in [1, 2] {
        assert_eq!(rig.shadow(peer).applied_tick(), Some(rig.tick));
        assert_eq!(rig.shadow(peer).world().world_digest(), rig.world.world_digest());
        assert!(rig.host.is_warm(PeerId(peer)));
    }
    assert!(rig.refusals.is_empty(), "{:?}", rig.refusals);
}

/// Successor change: when the first successor leaves, the second takes its place with its shadow
/// intact (no full state is loaded again) and the next best client is appointed and seeded.
#[test]
fn the_second_successor_takes_over_when_the_first_leaves() {
    let mut rig = Rig::new(config(), &[(1, 10.0), (2, 20.0), (3, 30.0), (4, 40.0)], 2_000);
    rig.run(400);
    assert_eq!(rig.host.successors(), vec![PeerId(1), PeerId(2)]);
    assert_eq!(rig.shadow(2).rank(), Some(1));
    let reloads_before = rig.shadow(2).status().reloads;
    rig.events.clear();
    rig.remove(1);
    rig.run(1);
    assert_eq!(rig.host.successors(), vec![PeerId(2), PeerId(3)]);
    assert!(
        rig.events.contains(&JournalEvent::Dismissed {
            peer: PeerId(1),
            reason: "left"
        }),
        "{:?}",
        rig.events
    );
    assert!(rig.events.contains(&JournalEvent::Appointed {
        peer: PeerId(2),
        rank: 0
    }));
    assert!(rig.events.contains(&JournalEvent::Appointed {
        peer: PeerId(3),
        rank: 1
    }));
    assert_eq!(
        rig.shadow(2).rank(),
        Some(0),
        "the second successor is told it is first"
    );
    assert!(rig.shadow(2).promotable(), "and never stopped being warm");
    rig.run(120);
    assert_eq!(
        rig.shadow(2).status().reloads,
        reloads_before,
        "its shadow was not seeded again"
    );
    assert_eq!(rig.shadow(2).applied_tick(), Some(rig.tick));
    let third = rig.shadow(3).status();
    assert_eq!((third.state.as_str(), third.rank), ("warm", Some(1)));
    assert_eq!(rig.shadow(3).applied_tick(), Some(rig.tick));
    assert_eq!(rig.shadow(3).world().world_digest(), rig.world.world_digest());
    assert!(rig.refusals.is_empty(), "{:?}", rig.refusals);
    assert_eq!(
        rig.host.status().successor_changes,
        2,
        "the first appointment and this change"
    );
    // Both leave: the last two clients are appointed; with nobody left the journal is idle.
    rig.remove(2);
    rig.remove(3);
    rig.run(60);
    assert_eq!(rig.host.successors(), vec![PeerId(4)]);
    assert!(rig.host.is_warm(PeerId(4)));
    rig.remove(4);
    rig.run(2);
    assert!(rig.host.successors().is_empty());
    assert_eq!(rig.host.status().state, "no_successor");
    assert!(!rig.host.full_wanted(rig.now_ms), "no successor, no full states");
}

/// Best connected: interrupted clients are never picked and are replaced when sitting; a
/// candidate that is only a little better does not unseat a successor, one that is far better
/// does at the next comparison; a dismissed client drops its shadow.
#[test]
fn the_best_connected_clients_are_successors_with_hysteresis() {
    let lossy = Candidate {
        loss_fraction: 0.05,
        ..candidate(9, 10.0)
    };
    let silent = Candidate {
        interrupted: true,
        ..candidate(8, 1.0)
    };
    assert_eq!(
        rank_candidates(&[
            candidate(3, 40.0),
            lossy,
            silent,
            candidate(2, 40.0),
            candidate(5, 20.0)
        ]),
        vec![PeerId(5), PeerId(2), PeerId(3), PeerId(9), PeerId(8)],
        "score = rtt + jitter + 10 ms per percent of loss; ties by id; silent last"
    );
    assert!(candidate(1, f32::NAN).score_ms() > 1_000.0);

    let mut rig = Rig::new(config(), &[(1, 50.0), (2, 60.0), (3, 300.0)], 0);
    rig.run(120);
    assert_eq!(rig.host.successors(), vec![PeerId(1), PeerId(2)]);
    // Client 3 becomes a little better than client 2: no change, even after many comparisons.
    rig.candidates[2].rtt_ms = 45.0;
    rig.run(1_300);
    assert_eq!(rig.host.successors(), vec![PeerId(1), PeerId(2)]);
    // Far better (under half the score and 20 ms better, once its smoothed score has followed):
    // it takes the worst one's place within two comparisons.
    rig.candidates[2].rtt_ms = 5.0;
    rig.run(120);
    assert_eq!(
        rig.host.successors(),
        vec![PeerId(1), PeerId(2)],
        "two seconds of a better link unseat nobody"
    );
    rig.run(1_200);
    assert_eq!(rig.host.successors(), vec![PeerId(1), PeerId(3)]);
    assert!(rig.events.contains(&JournalEvent::Dismissed {
        peer: PeerId(2),
        reason: "outranked"
    }));
    assert_eq!(
        rig.shadow(2).status().role,
        "client",
        "the dismissed client was told"
    );
    assert!(!rig.shadow(2).promotable());
    assert!(rig.host.is_warm(PeerId(3)));
    // A starved link ranks after every link that is not, whatever the round trips; a sitting
    // successor that stays warm on one keeps its place.
    let mut starving = Rig::new(config(), &[(1, 10.0), (2, 20.0), (3, 90.0)], 0);
    starving.run(120);
    assert_eq!(starving.host.successors(), vec![PeerId(1), PeerId(2)]);
    starving.candidates[0].allowance_bytes_per_s = Some(4_095);
    starving.candidates[1].allowance_bytes_per_s = Some(60_000);
    starving.candidates[2].allowance_bytes_per_s = Some(60_000);
    assert!(starving.candidates[0].starved());
    assert_eq!(
        rank_candidates(&starving.candidates),
        vec![PeerId(2), PeerId(3), PeerId(1)],
        "a starved link ranks after every link that is not"
    );
    starving.run(1_300);
    assert_eq!(
        starving.host.successors(),
        vec![PeerId(1), PeerId(2)],
        "warm successors stay"
    );
    // Its budget stops carrying even the log: the shadow falls out of the host's ring, and at
    // the next comparison a client whose link is not starved takes its place.
    starving.allowance_of.insert(1, 40);
    starving.run(2_000);
    assert_eq!(starving.host.successors(), vec![PeerId(2), PeerId(3)]);
    assert!(
        starving.events.contains(&JournalEvent::Dismissed {
            peer: PeerId(1),
            reason: "fell_behind"
        }),
        "{:?}",
        starving.events
    );
    assert!(starving.host.is_warm(PeerId(3)));
    assert!(starving.refusals.is_empty(), "{:?}", starving.refusals);
    // With every link starved the best of them is still appointed.
    let mut all = Rig::new(config(), &[(1, 10.0), (2, 20.0)], 0);
    for c in &mut all.candidates {
        c.allowance_bytes_per_s = Some(4_095);
    }
    all.run(10);
    assert_eq!(all.host.successors(), vec![PeerId(1), PeerId(2)]);
    // The first successor goes silent: replaced at once by the best client that is not.
    rig.candidates[0].interrupted = true;
    rig.run(2);
    assert_eq!(rig.host.successors(), vec![PeerId(3), PeerId(2)]);
    assert!(rig.events.contains(&JournalEvent::Dismissed {
        peer: PeerId(1),
        reason: "interrupted"
    }));
    rig.run(60);
    assert!(rig.host.is_warm(PeerId(2)));
    assert!(rig.refusals.is_empty(), "{:?}", rig.refusals);
}

/// Budget: with a 64 KiB/s allowance the journal spends a quarter, 16 KiB/s. A 300 KiB full
/// state then takes about 21 s (the log takes 1 KiB/s and a part's header 6 % at this rate), in parts as large as one tick's budget pays for (never a burst
/// the stream would hold in front of the log); the log keeps flowing ahead of it, no second is
/// over its budget plus the two-tick burst, and the total never exceeds the budget accrued.
#[test]
fn the_journal_stays_inside_its_budget_share_and_the_log_goes_first() {
    let mut rig = Rig::new(
        JournalConfig {
            successors: 1,
            depth_ticks: 3_600,
            ..config()
        },
        &[(1, 10.0)],
        300 * 1024,
    );
    rig.allowance = Some(64 * 1024);
    let rate = 16 * 1024_u64;
    // What an idle bucket holds at most: one payload (two ticks of the rate are less here).
    let burst = JOURNAL_PART_BYTES as u64;
    let mut seeded_at = None;
    for _ in 0..3_000 {
        rig.step();
        if seeded_at.is_none() && rig.shadow(1).promotable() {
            seeded_at = Some(rig.now_ms);
        }
        let row = &rig.host.status().successors[0];
        assert!(row.bytes_sent <= row.budget_bytes + 1, "{row:?}");
        // One poll hands the transport one payload of at most 900 bytes: never a burst the
        // stream would hold in front of the next tick's log.
        assert!(
            rig.last_handed <= JOURNAL_PART_BYTES as u64,
            "tick {}: {} B in one poll",
            rig.tick,
            rig.last_handed
        );
        // The log is never queued behind the full state: at most this tick's record waits.
        assert!(row.queued_ticks <= 1, "tick {}: {row:?}", rig.tick);
    }
    let seeded_at = seeded_at.expect("the 300 KiB full state arrives");
    assert!(
        (20_800..=22_000).contains(&seeded_at),
        "300 KiB at 16 KiB/s next to the log, 241 B of it a tick: seeded at {seeded_at} ms"
    );
    for ((_, second), bytes) in &rig.sent {
        assert!(
            *bytes <= rate + burst,
            "second {second}: {bytes} journal bytes against {rate} + {burst}"
        );
    }
    let total: u64 = rig.sent.values().sum();
    assert!(total <= rate * 50, "{total} bytes in 50 s at {rate} B/s");
    let row = &rig.host.status().successors[0];
    assert_eq!(row.seconds_over_budget, 0);
    assert_eq!(row.budget_bytes_per_s, rate);
    assert_eq!(row.bytes_sent, total);
    assert!(rig.refusals.is_empty(), "{:?}", rig.refusals);
    assert_eq!(rig.shadow(1).applied_tick(), Some(rig.tick));
    assert_eq!(rig.shadow(1).world().world_digest(), rig.world.world_digest());
    // A link without a rate controller gets the fallback allowance's share.
    let mut rig = Rig::new(config(), &[(1, 10.0)], 0);
    rig.allowance = None;
    rig.run(120);
    assert_eq!(
        rig.host.status().successors[0].budget_bytes_per_s,
        u64::from(JOURNAL_FALLBACK_BYTES_PER_S) / 4
    );
}

/// A journal that leaves an event out does not reproduce the host's world: the shadow counts the
/// divergence at the next full-state tick, loads that full state, and is the host's world again.
#[test]
fn a_log_that_misses_an_event_is_counted_and_the_full_state_repairs_it() {
    let mut rig = Rig::new(config(), &[(1, 10.0)], 500);
    rig.forget_events = true;
    rig.run(1_000);
    let status = rig.shadow(1).status();
    assert!(status.replay_divergences >= 2, "{status:?}");
    assert_eq!(
        status.reloads, status.full_states,
        "every full state had to be loaded"
    );
    assert_eq!(
        status.digest_mismatches, 0,
        "the full states themselves are right"
    );
    assert_eq!(status.state, "warm");
    for (tick, digest) in &rig.full_digests {
        assert_eq!(
            rig.shadow(1).world().loaded.get(tick),
            Some(digest),
            "tick {tick}"
        );
    }
    let host = rig.host.status();
    assert_eq!(
        host.replay_divergences, status.replay_divergences,
        "the host sees the count"
    );
    assert_eq!(host.successors[0].state, "warm");
}

/// A full state that does not have the digest sent with it is refused
/// (`journal_digest_mismatch`), the shadow is stale, and the host asks for another full state a
/// second later, which seeds it.
#[test]
fn a_full_state_with_a_wrong_digest_is_refused_and_replaced() {
    let mut rig = Rig::new(config(), &[(1, 10.0)], 500);
    rig.wrong_digests = 1;
    rig.run(30);
    assert_eq!(rig.refusals.first(), Some(&(1, "journal_digest_mismatch")));
    let status = rig.shadow(1).status();
    assert_eq!((status.state.as_str(), status.digest_mismatches), ("stale", 1));
    assert!(!status.promotable);
    assert_eq!(rig.host.status().successors[0].state, "stale");
    assert_eq!(rig.full_digests.len(), 1, "no second full state inside a second");
    rig.run(60);
    assert_eq!(
        rig.full_digests.len(),
        2,
        "one extra full state, a second after the first"
    );
    let status = rig.shadow(1).status();
    assert_eq!(status.state, "warm");
    assert_eq!(rig.shadow(1).applied_tick(), Some(rig.tick));
    assert_eq!(rig.shadow(1).world().world_digest(), rig.world.world_digest());
    let host = rig.host.status();
    assert_eq!(host.digest_mismatches, 1);
    assert_eq!(host.successors[0].state, "warm");
    // Refusals between the mismatch and the new seed are the log the shadow had no base for.
    assert!(
        rig.refusals[1..]
            .iter()
            .all(|(_, code)| *code == "journal_no_base"),
        "{:?}",
        rig.refusals
    );
}

/// With a one-tick pipe each way the host reads a lag of two ticks (one out, one back) and no
/// more; payloads the transport refused are held and go first.
#[test]
fn the_host_reads_the_lag_from_acknowledgments_and_held_payloads_go_first() {
    let mut rig = Rig::new(config(), &[(1, 10.0)], 0);
    rig.delay_ticks = 1;
    rig.run(900);
    let row = &rig.host.status().successors[0];
    assert_eq!((row.lag_ticks, row.lag_ticks_p99), (2, 2), "{row:?}");
    assert!(row.lag_ticks_max <= 3, "{row:?}");
    assert_eq!(rig.host.status().journal_lag_ticks_p99, 2);
    assert_eq!(rig.shadow(1).status().journal_lag_ticks, 0);

    let mut host = JournalHost::new(config());
    host.set_candidates(0, &[candidate(1, 10.0)]);
    host.record_tick(1, &[1], &[], Some([1; DIGEST_BYTES]))
        .expect("tick");
    host.record_full(0, 1, vec![1, 2, 3], [1; DIGEST_BYTES])
        .expect("full");
    assert!(
        host.poll(0, |_| Some(1 << 20)).is_empty(),
        "the bucket starts empty"
    );
    let first = host.poll(100, |_| Some(1 << 20));
    assert_eq!(first.len(), 1, "one payload for the successor");
    let msgs = decode_all(&first[0].bytes).expect("the payload's messages");
    assert!(
        matches!(
            msgs[..],
            [Msg::Appoint { .. }, Msg::FullBegin { .. }, Msg::FullData { .. }]
        ),
        "the appointment, the full state's start and its bytes, in one payload"
    );
    // The transport refused it.
    host.hold(first[0].clone());
    host.record_tick(2, &[2], &[], None).expect("tick");
    let again = host.poll(116, |_| Some(1 << 20));
    assert_eq!(again, first, "the held payload goes first, alone");
    let next = host.poll(133, |_| Some(1 << 20));
    assert_eq!(next.len(), 1, "then the log goes on");
    assert!(matches!(decode(&next[0].bytes), Ok(Msg::Log { tick_low: 2, .. })));
}

/// One fragment at a time: what a poll hands a successor is one payload of at most 900 bytes,
/// and `poll_links` hands out the next only when the stream has acknowledged the one before
/// (one may be on its way while the allowance is healthy; none under 32 KiB/s). The log that
/// waited goes in one message.
#[test]
fn a_new_payload_waits_for_the_stream_to_acknowledge_the_last() {
    let mut host = JournalHost::new(config());
    host.set_candidates(0, &[candidate(1, 10.0)]);
    let mut tick = 0_u64;
    let mut step = |host: &mut JournalHost, allowance: u32, in_flight: usize| {
        tick += 1;
        let now = tick * 1_000 / 60;
        if host.full_wanted(now) {
            host.record_tick(tick, &[7; 40], &[], Some([2; DIGEST_BYTES]))
                .expect("tick");
            host.record_full(now, tick, vec![9; 5_000], [2; DIGEST_BYTES])
                .expect("full");
        } else {
            host.record_tick(tick, &[7; 40], &[], None).expect("tick");
        }
        let out = host.poll_links(now, |_| JournalLink {
            allowance_bytes_per_s: Some(allowance),
            in_flight_fragments: in_flight,
        });
        assert!(out.len() <= 1, "one payload a poll");
        for o in &out {
            assert!(
                o.bytes.len() <= JOURNAL_PART_BYTES,
                "{} B in one payload",
                o.bytes.len()
            );
        }
        out
    };
    let healthy = 64 * 1024;
    let mut sent = 0;
    for _ in 0..30 {
        sent += step(&mut host, healthy, 0).len();
    }
    assert!(
        sent >= 28,
        "a payload every tick while the stream is clear: {sent}"
    );
    assert_eq!(
        step(&mut host, healthy, 1).len(),
        1,
        "one fragment may be on its way"
    );
    assert!(step(&mut host, healthy, 2).is_empty(), "two may not");
    assert!(step(&mut host, healthy, 2).is_empty());
    // The two ticks that waited leave with the next payload, in one log message.
    let out = step(&mut host, healthy, 1);
    let msgs = decode_all(&out[0].bytes).expect("messages");
    assert!(
        matches!(&msgs[0], Msg::Log { records, .. } if records.len() == 3),
        "the log of the ticks that waited and of this one"
    );
    // Under 32 KiB/s of allowance the journal is stop-and-wait.
    let starved = 8 * 1024;
    assert!(starved < JOURNAL_STOP_AND_WAIT_BYTES_PER_S);
    for _ in 0..10 {
        assert!(
            step(&mut host, starved, 1).is_empty(),
            "nothing new behind an unacknowledged fragment"
        );
    }
    let out = step(&mut host, starved, 0);
    assert_eq!(out.len(), 1, "the stream is clear: the next payload leaves");
    let msgs = decode_all(&out[0].bytes).expect("messages");
    assert!(matches!(&msgs[0], Msg::Log { records, .. } if records.len() == 11));
    assert_eq!(host.status().successors[0].queued_ticks, 0);
    assert_eq!(host.status().successors[0].seconds_over_budget, 0);
}

/// Ticks go up by one. A tick at or before the newest is refused; after a jump the log starts
/// again and the successors are seeded from a full state taken after it.
#[test]
fn a_tick_jump_restarts_the_journal() {
    let mut rig = Rig::new(config(), &[(1, 10.0)], 0);
    rig.run(100);
    assert_eq!(
        rig.host
            .record_tick(100, &[], &[], None)
            .expect_err("not after")
            .code(),
        "journal_tick_not_after"
    );
    assert_eq!(
        rig.host
            .record_full(rig.now_ms, 99, Vec::new(), [0; DIGEST_BYTES])
            .expect_err("not the newest tick")
            .code(),
        "journal_full_tick_unknown"
    );
    assert_eq!(
        rig.host
            .record_tick(101, &vec![0; MAX_JOURNAL_RECORD_BYTES + 1], &[], None)
            .expect_err("too large")
            .code(),
        "journal_record_too_large"
    );
    assert_eq!(
        rig.host
            .record_full(
                rig.now_ms,
                100,
                vec![0; MAX_JOURNAL_FULL_BYTES + 1],
                [0; DIGEST_BYTES]
            )
            .expect_err("too large")
            .code(),
        "journal_full_too_large"
    );
    // The host's simulation jumps 50 ticks (a level change, say).
    rig.tick += 50;
    rig.world.tick = rig.tick;
    assert!(rig.host.status().log_ticks > 0);
    rig.run(30);
    assert!(rig.host.status().log_ticks <= 30, "the ring restarted");
    assert_eq!(rig.shadow(1).applied_tick(), Some(rig.tick));
    assert_eq!(rig.shadow(1).world().world_digest(), rig.world.world_digest());
    assert!(rig.refusals.is_empty(), "{:?}", rig.refusals);
    assert_eq!(rig.shadow(1).status().reloads, 2, "seeded again after the jump");
}

/// Hostile and broken payloads: each is refused with its code, nothing is stored past the caps,
/// and 20,000 seeded mutations of every payload kind decode on both sides without a panic.
#[test]
fn hostile_payloads_are_refused_with_codes_and_mutations_never_panic() {
    let world = ToyWorld::with_blob(20_000);
    let full = FullState {
        seq: 1,
        tick: 0,
        bytes: world.encode(),
        digest: world.world_digest(),
    };
    let record = JournalRecord {
        tick: 1,
        inputs: vec![1; 40].into(),
        events: vec![2; 40].into(),
        digest: Some([3; DIGEST_BYTES]),
    };
    let log = encode_log(1, 1, [record.clone()].iter());
    let parts = full_payloads(1, &full, 8 * 1024);
    assert_eq!(parts.len(), 4);
    let part = |i: usize| parts[i].clone();
    let code = |r: Result<(), JournalError>| r.expect_err("refused").code();

    // Not a successor: journal data is refused and nothing is acknowledged.
    let mut idle = JournalSuccessor::new(ToyWorld::default());
    assert_eq!(code(idle.on_payload(&part(0))), "journal_not_successor");
    assert_eq!(code(idle.on_payload(&log)), "journal_not_successor");
    assert!(idle.poll_ack().is_none());
    assert_eq!(idle.status().refused, 2);

    let appointed = || {
        let mut s = JournalSuccessor::new(ToyWorld::default());
        s.on_payload(&encode_appoint(1, 0, 600)).expect("appoint");
        s
    };
    // A log before any full state; an acknowledgment sent to a client; another appointment's data.
    let mut s = appointed();
    assert_eq!(code(s.on_payload(&log)), "journal_no_base");
    assert_eq!(
        code(s.on_payload(&encode_ack(&Ack {
            appointment: 1,
            state: ShadowState::Warm,
            applied_tick: 0,
            seen_seq: 0,
            digest_mismatches: 0,
            replay_divergences: 0,
            refused: 0,
        }))),
        "journal_unexpected"
    );
    assert_eq!(
        code(s.on_payload(&encode_full_begin(2, &full, true))),
        "journal_stale_appointment"
    );
    assert_eq!(code(s.on_payload(&[KIND_LOG, 1, 2])), "journal_malformed");
    assert_eq!(code(s.on_payload(&[0x7F])), "journal_malformed");
    // Parts out of order, and a part with no start.
    assert_eq!(code(s.on_payload(&part(1))), "journal_full_out_of_order");
    s.on_payload(&part(0)).expect("part 0");
    s.on_payload(&part(1)).expect("part 1");
    assert_eq!(code(s.on_payload(&part(3))), "journal_full_out_of_order");
    assert_eq!(s.status().state, "stale");
    // A gap in the log.
    let mut s = appointed();
    for bytes in &parts {
        s.on_payload(bytes).expect("part");
    }
    assert!(s.promotable());
    let skip = JournalRecord {
        tick: 3,
        ..record.clone()
    };
    assert_eq!(
        code(s.on_payload(&encode_log(1, 3, [skip].iter()))),
        "journal_gap"
    );
    assert_eq!(s.status().state, "stale");
    // A full state that declares more than the cap, or carries more than it declared: nothing
    // is stored for the first, and the second is refused at the part that passes the total.
    let mut s = appointed();
    let mut w = WireWriter::new();
    w.u8(KIND_FULL);
    w.u32(1);
    w.u32(1);
    w.u8(1);
    w.u64(0);
    w.u32(MAX_JOURNAL_FULL_BYTES as u32 + 1);
    w.raw(&[0; DIGEST_BYTES]);
    assert_eq!(code(s.on_payload(&w.finish())), "journal_malformed");
    let small = FullState {
        seq: 1,
        tick: 0,
        bytes: vec![7; 100],
        digest: [0; DIGEST_BYTES],
    };
    let declared_50 = FullState {
        bytes: vec![7; 50],
        ..FullState {
            seq: 1,
            tick: 0,
            bytes: Vec::new(),
            digest: [0; DIGEST_BYTES],
        }
    };
    s.on_payload(&encode_full_begin(1, &declared_50, true))
        .expect("the start");
    assert_eq!(
        code(s.on_payload(&encode_full_data(1, &small, 0, 60))),
        "journal_full_out_of_order",
        "a part past the declared total"
    );
    assert_eq!(s.status().full_state_bytes, 0, "nothing of it is kept");
    // A shadow world that refuses the bytes.
    let mut s = appointed();
    let garbage = FullState {
        seq: 1,
        tick: 0,
        bytes: vec![1, 2, 3],
        digest: [0; DIGEST_BYTES],
    };
    s.on_payload(&encode_full_begin(1, &garbage, true))
        .expect("the start");
    assert_eq!(
        code(s.on_payload(&encode_full_data(1, &garbage, 0, 3))),
        "journal_load_failed"
    );

    // The host takes acknowledgments only, and only from its successors.
    let mut host = JournalHost::new(config());
    host.set_candidates(0, &[candidate(1, 10.0)]);
    let ack = |appointment| {
        encode_ack(&Ack {
            appointment,
            state: ShadowState::Warm,
            applied_tick: 5,
            seen_seq: 1,
            digest_mismatches: 0,
            replay_divergences: 0,
            refused: 0,
        })
    };
    assert_eq!(code(host.on_payload(PeerId(2), &ack(1))), "journal_not_successor");
    assert_eq!(
        code(host.on_payload(PeerId(1), &ack(9))),
        "journal_stale_appointment"
    );
    assert_eq!(code(host.on_payload(PeerId(1), &log)), "journal_unexpected");
    assert_eq!(
        code(host.on_payload(PeerId(1), &[KIND_ACK, 0])),
        "journal_malformed"
    );
    host.on_payload(PeerId(1), &ack(1))
        .expect("its successor's acknowledgment");
    let status = host.status();
    assert_eq!(status.refused, 4);
    assert_eq!(status.last_refusal.as_deref(), Some("journal_malformed"));

    // Seeded mutations of every payload kind, on a successor mid-journal and on the host.
    let seeds: Vec<Vec<u8>> = vec![
        encode_appoint(1, 1, 600),
        encode_dismiss(1),
        part(0),
        part(1),
        part(3),
        log.clone(),
        ack(1),
    ];
    let mut state = 0xC0FF_EE00_0000_0001_u64;
    let mut s = appointed();
    let mut refused = 0_u64;
    for i in 0..20_000 {
        for seed in &seeds {
            let mut bytes = seed.clone();
            let r = rng(&mut state);
            match r % 4 {
                0 => bytes.truncate((r >> 8) as usize % (bytes.len() + 1)),
                1 => {
                    let at = (r >> 8) as usize % bytes.len();
                    bytes[at] ^= 1 << ((r >> 40) % 8);
                }
                2 => {
                    let at = (r >> 8) as usize % bytes.len();
                    bytes[at] = (r >> 48) as u8;
                    bytes.extend_from_slice(&r.to_le_bytes()[..(r >> 60) as usize % 8]);
                }
                _ => {
                    for _ in 0..3 {
                        let r = rng(&mut state);
                        let at = (r >> 8) as usize % bytes.len();
                        bytes[at] = (r >> 48) as u8;
                    }
                }
            }
            if s.on_payload(&bytes).is_err() {
                refused += 1;
            }
            let _ = host.on_payload(PeerId(1), &bytes);
            let status = s.status();
            assert!(status.log_ticks <= 600, "iteration {i}: {status:?}");
            assert!(status.full_state_bytes <= MAX_JOURNAL_FULL_BYTES as u64);
        }
        if i % 97 == 0 {
            // Let the journal get somewhere between mutations.
            let _ = s.on_payload(&encode_appoint(1, 0, 600));
            let _ = s.on_payload(&part(0));
            let _ = s.on_payload(&part(1));
        }
    }
    assert!(refused > 20_000, "most mutations are refused: {refused}");
}

/// A successor the host seeds again while it still holds an older journal (it fell out of the
/// host's ring) drops that journal at the new full state's start: the log that follows the start
/// is taken, not read as a gap after the old log. A full state on the cadence does not restart
/// the log.
#[test]
fn a_reseed_restarts_the_log_and_a_cadence_full_state_does_not() {
    let world_at = |tick: u64| ToyWorld {
        tick,
        ..ToyWorld::default()
    };
    let full_at = |seq: u32, tick: u64| FullState {
        seq,
        tick,
        bytes: world_at(tick).encode(),
        digest: world_at(tick).world_digest(),
    };
    let record = |tick: u64| JournalRecord {
        tick,
        inputs: Vec::new().into(),
        events: Vec::new().into(),
        digest: None,
    };
    let mut shadow = JournalSuccessor::new(ToyWorld::default());
    shadow.on_payload(&encode_appoint(1, 0, 600)).expect("appoint");
    for bytes in full_payloads(1, &full_at(1, 10), JOURNAL_PART_BYTES) {
        shadow.on_payload(&bytes).expect("first seed");
    }
    for tick in 11..=20 {
        shadow
            .on_payload(&encode_log(1, tick, [record(tick)].iter()))
            .expect("log");
    }
    assert_eq!(shadow.applied_tick(), Some(20));
    // The host seeds it again from tick 5,000: the start, then the log, then the bytes.
    let again = full_payloads(1, &full_at(2, 5_000), JOURNAL_PART_BYTES);
    shadow.on_payload(&again[0]).expect("the start of the new seed");
    assert_eq!(shadow.status().state, "seeding");
    assert!(!shadow.promotable(), "the old journal is dropped");
    shadow
        .on_payload(&encode_log(1, 5_001, [record(5_001)].iter()))
        .expect("the log after the new seed is not a gap");
    for bytes in &again[1..] {
        shadow.on_payload(bytes).expect("the bytes");
    }
    assert_eq!(shadow.applied_tick(), Some(5_001));
    assert_eq!(shadow.status().refused, 0);
    // A full state on the cadence: the log goes on through it.
    shadow
        .on_payload(&encode_log(1, 5_002, [record(5_002)].iter()))
        .expect("log");
    let cadence = full_at(3, 5_002);
    shadow
        .on_payload(&encode_full_begin(1, &cadence, false))
        .expect("the start of a cadence full state");
    assert!(shadow.promotable(), "the shadow stays warm while it arrives");
    shadow
        .on_payload(&encode_log(1, 5_003, [record(5_003)].iter()))
        .expect("the log goes on");
    shadow
        .on_payload(&encode_full_data(1, &cadence, 0, cadence.bytes.len()))
        .expect("the bytes");
    assert_eq!(shadow.applied_tick(), Some(5_003));
    assert_eq!(shadow.base_tick(), Some(5_002));
    assert_eq!(shadow.status().log_ticks, 1);
    assert_eq!(shadow.world().world_digest(), world_at(5_003).world_digest());
    assert_eq!(shadow.status().refused, 0);
}

/// The agent bridge keeps what hosts and successors publish for `net.migration`.
#[test]
fn hosts_and_successors_publish_their_status() {
    let mut rig = Rig::new(config(), &[(1, 10.0), (2, 20.0)], 0);
    rig.run(120);
    let mut state = NetAgentState::default();
    rig.host.publish("server", &mut state);
    rig.shadow(1).publish("client1", &mut state);
    assert_eq!(state.migration().len(), 2);
    let host = &state.migration()["server"];
    assert_eq!(host.role, "host");
    assert_eq!(host.successors.len(), 2);
    assert_eq!(state.migration()["client1"].rank, Some(0));
    let json = serde_json_like(host);
    assert!(
        json.contains("successors") && json.contains("journal_lag_ticks"),
        "{json}"
    );
    state.remove_migration("client1");
    assert_eq!(state.migration().len(), 1);
    for i in 0..100 {
        state.publish_migration(&format!("label{i}"), JournalStatus::default());
    }
    assert_eq!(state.migration().len(), 64, "the bridge keeps at most 64 labels");
}

/// The status as RON text (the crate has no JSON dependency; the field names are the same).
fn serde_json_like(status: &JournalStatus) -> String {
    ron::to_string(status).expect("status serializes")
}
