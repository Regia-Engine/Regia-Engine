// SPDX-License-Identifier: MPL-2.0
//! Replay determinism: for many seeds, a synthetic recording parses to the same file every time, any
//! way of stepping or seeking reaches the same state and delivers the same events, and Moment
//! extraction is stable. No wall-clock input and no files: bytes come from a seeded generator.

use regia_core::math::Vec3;
use regia_framework::replay_sim::{
    FullCodec, Moment, MomentKind, MomentQuery, NavBeacon, NavRecord, Participant, PeerCommand, PeerCommands,
    PeerRow, PilotInput, PlayerRow, ReplayEntity, ReplayEvent, ReplayFile, ReplayHeader, ReplaySim,
    SyntheticReplay, sim_to_regia,
};
use regia_framework::sim_bridge::ExternalSim;

const SEEDS: [u64; 6] = [0, 1, 7, 0xDEAD_BEEF, 0x1234_5678_9ABC_DEF0, u64::MAX];

/// A tiny xorshift64* generator; the seed fully determines the recording.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed ^ 0x9E37_79B9_7F4A_7C15)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Snapshots are 3-byte rows `id, team, x`.
struct RowCodec;

impl FullCodec for RowCodec {
    fn decode(&self, snapshot: &[u8]) -> Result<Vec<ReplayEntity>, String> {
        if !snapshot.len().is_multiple_of(3) {
            return Err("snapshot length is not a multiple of 3".into());
        }
        Ok(snapshot
            .as_chunks::<3>()
            .0
            .iter()
            .map(|c| ReplayEntity {
                id: u64::from(c[0]),
                kind: "warhull".into(),
                team: u32::from(c[1]),
                translation: sim_to_regia(Vec3::new(f32::from(c[2] as i8), 0.0, 0.0)),
                rotation: regia_core::math::Quat::IDENTITY,
            })
            .collect())
    }

    fn state_hash(&self, snapshot: &[u8]) -> Option<u64> {
        Some(snapshot.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
            (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
        }))
    }
}

fn header(seed: u64) -> ReplayHeader {
    ReplayHeader {
        version: 3,
        protocol: "test/1".into(),
        tick_rate: 60,
        lobby_preset: "alpha".into(),
        seed,
        bootstrap: "bg_a".into(),
        map: "ironreach".into(),
        mode: "tdm".into(),
        teams: 2,
        players: vec![PlayerRow {
            peer_id: 1,
            account_id: String::new(),
            callsign: "Nova".into(),
            role: 0,
            team: 0,
        }],
        input_delay: 3,
        peers: vec![PeerRow { peer_id: 1, role: 0 }],
        sim_git_sha: "unknown".into(),
        sim_abi: 7,
        content_hash: "c0ffee".into(),
        config_hash: String::new(),
    }
}

fn moment(rng: &mut Rng, tick: u32) -> Moment {
    let attacker = 1 + rng.below(4) as u32;
    let victim = 1 + rng.below(4) as u32;
    Moment {
        drained_tick: tick,
        tick: u64::from(tick),
        kind: MomentKind::from_wire(1 + rng.below(3) as u8),
        span_match: 1,
        span_round: 1 + rng.below(2) as u32,
        span_fight: 1 + rng.below(3) as u32,
        participants: [
            Participant {
                role: 1,
                entity_id: attacker,
            },
            Participant {
                role: 2,
                entity_id: victim,
            },
        ],
        position: Vec3::new(rng.below(100) as f32, 2.0, -3.25),
        damage: rng.below(200) as u32,
        weapon: if rng.below(2) == 0 {
            "plasma_cannon"
        } else {
            "rail"
        }
        .into(),
        attacker,
        victim,
        importance: rng.below(101) as u8,
    }
}

/// A recording of `seed`: inputs on every tick up to a seeded length, fulls every 30 ticks, and
/// Moments and commands on seeded ticks (always including tick 0 for a command).
fn recording(seed: u64) -> Vec<u8> {
    let mut rng = Rng::new(seed);
    let end = 200 + rng.below(300) as u32;
    let mut w = SyntheticReplay::new(&header(seed));
    for tick in 0..=end {
        let input = PilotInput {
            throttle: rng.below(256) as u8 as i8,
            strafe: 0,
            steer: 0,
            aim_pitch: 0,
            action_bits: rng.below(8) as u16,
            stance: 0,
        };
        w = w.inputs(tick, &[(1, input)]);
        if tick.is_multiple_of(30) {
            let snap = [1u8, 0, rng.below(100) as u8, 2, 1, rng.below(100) as u8];
            w = w.full(&snap, RowCodec.state_hash(&snap).unwrap_or(0));
            w = w.nav(&NavRecord {
                tick,
                beacons: vec![NavBeacon {
                    id: 9,
                    owner_slot: 1,
                    team: 0,
                    number: 3,
                    x_cm: rng.below(5000) as i32,
                    y_cm: 0,
                    z_cm: 0,
                    health: 100,
                }],
                waypoints: vec![],
            });
        }
        if tick == 0 || rng.below(15) == 0 {
            w = w.commands(&PeerCommands {
                tick,
                commands: vec![PeerCommand {
                    peer_id: 1,
                    ok: true,
                    kind: "build".into(),
                    text: format!("t{tick}"),
                }],
            });
        }
        if rng.below(10) == 0 {
            let m = moment(&mut rng, tick);
            w = w.moments(tick, &[m]);
        }
    }
    w.finish()
}

fn load(bytes: &[u8]) -> ReplaySim {
    let mut sim = ReplaySim::with_codec(RowCodec);
    sim.load_bytes(bytes).expect("synthetic recording parses");
    sim
}

fn events_of(sim: &mut ReplaySim) -> Vec<ReplayEvent> {
    let mut out = Vec::new();
    sim.drain_events(&mut out);
    out
}

fn event_tick(event: &ReplayEvent) -> u32 {
    match event {
        ReplayEvent::Moment(m) => m.drained_tick,
        ReplayEvent::Command { tick, .. } => *tick,
        ReplayEvent::Abort(a) => a.tick,
    }
}

#[test]
fn same_seed_gives_the_same_bytes_and_the_same_parse() {
    for seed in SEEDS {
        let a = recording(seed);
        assert_eq!(a, recording(seed), "seed {seed:#x}: bytes");
        let fa = ReplayFile::parse(&a).expect("parse");
        let fb = ReplayFile::parse(&a).expect("parse again");
        assert_eq!(fa.header, fb.header);
        assert_eq!(fa.moments, fb.moments);
        assert_eq!(fa.end_tick(), fb.end_tick());
        assert_eq!(fa.header.seed, seed, "the header seed round-trips");
    }
    assert_ne!(recording(1), recording(2), "different seeds differ");
}

#[test]
fn every_recording_verifies_with_its_codec() {
    for seed in SEEDS {
        let report = load(&recording(seed)).verify().expect("loaded");
        assert!(report.is_ok(), "seed {seed:#x}: {report:?}");
    }
}

#[test]
fn chunking_does_not_change_state_or_events() {
    for seed in SEEDS {
        let bytes = recording(seed);
        let mut whole = load(&bytes);
        let end = whole.end_tick();
        whole.step(end);
        let want_events = events_of(&mut whole);
        let want_snap = whole.snapshot();
        for chunk in [1u32, 2, 7, 30, 61] {
            let mut sim = load(&bytes);
            let mut got = Vec::new();
            while sim.tick() < end {
                sim.step(chunk);
                got.extend(events_of(&mut sim));
            }
            assert_eq!(got, want_events, "seed {seed:#x} chunk {chunk}: events");
            assert_eq!(
                sim.snapshot(),
                want_snap,
                "seed {seed:#x} chunk {chunk}: snapshot"
            );
        }
    }
}

#[test]
fn seek_then_step_matches_running_from_the_start() {
    for seed in SEEDS {
        let bytes = recording(seed);
        let mut rng = Rng::new(seed ^ 0x55);
        let end = load(&bytes).end_tick();
        for _ in 0..8 {
            let n = rng.below(u64::from(end) + 1) as u32;
            let m = rng.below(u64::from(end) + 1) as u32;
            let mut straight = load(&bytes);
            straight.step(n + m);
            let straight_events = events_of(&mut straight);

            let mut seeked = load(&bytes);
            // Wander first so the seek cannot lean on leftover state.
            seeked.step(end / 2);
            seeked.seek(end);
            seeked.seek(n);
            assert!(
                events_of(&mut seeked).is_empty(),
                "seed {seed:#x}: a seek fires nothing"
            );
            seeked.step(m);
            let seeked_events = events_of(&mut seeked);

            assert_eq!(seeked.tick(), straight.tick(), "seed {seed:#x} n {n} m {m}: tick");
            assert_eq!(
                seeked.snapshot(),
                straight.snapshot(),
                "seed {seed:#x} n {n} m {m}: snapshot"
            );
            let after_n: Vec<ReplayEvent> = straight_events
                .into_iter()
                .filter(|e| event_tick(e) > n)
                .collect();
            assert_eq!(
                seeked_events, after_n,
                "seed {seed:#x} n {n} m {m}: events past the seek"
            );
        }
    }
}

#[test]
fn events_stamped_tick_zero_arrive_on_the_first_step() {
    for seed in SEEDS {
        let mut sim = load(&recording(seed));
        sim.step(1);
        let events = events_of(&mut sim);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ReplayEvent::Command { tick: 0, .. })),
            "seed {seed:#x}: the tick 0 command is delivered"
        );
        sim.step(0);
        assert!(events_of(&mut sim).is_empty(), "and only once");
    }
}

#[test]
fn rewinding_and_replaying_delivers_the_same_events() {
    for seed in SEEDS {
        let mut sim = load(&recording(seed));
        let end = sim.end_tick();
        sim.step(end);
        let first = events_of(&mut sim);
        sim.seek(0);
        sim.step(end);
        let second = events_of(&mut sim);
        // The seek to 0 fires nothing, so the tick 0 events are the only difference.
        let without_zero: Vec<ReplayEvent> = first.into_iter().filter(|e| event_tick(e) > 0).collect();
        assert_eq!(second, without_zero, "seed {seed:#x}");
    }
}

#[test]
fn moment_extraction_is_stable_and_matches_delivery() {
    for seed in SEEDS {
        let bytes = recording(seed);
        let file = ReplayFile::parse(&bytes).expect("parse");
        let all = file.moments_matching(&MomentQuery::default());
        assert_eq!(
            all,
            (0..file.moments.len()).collect::<Vec<_>>(),
            "default query takes all, in order"
        );
        assert!(
            file.moments
                .windows(2)
                .all(|w| w[0].drained_tick <= w[1].drained_tick),
            "seed {seed:#x}: file order is tick order"
        );

        let kills = MomentQuery {
            kind: Some(MomentKind::Kill),
            ..MomentQuery::default()
        };
        let again = ReplayFile::parse(&bytes).expect("parse again");
        assert_eq!(file.moments_matching(&kills), again.moments_matching(&kills));
        // A filter is a subset, in the same order.
        let picked = file.moments_matching(&kills);
        assert!(picked.windows(2).all(|w| w[0] < w[1]));
        assert!(picked.iter().all(|&i| file.moments[i].kind == MomentKind::Kill));
        // Windows partition the stream: [0, mid] and [mid + 1, end] cover everything once.
        let mid = u64::from(file.end_tick() / 2);
        let lo = file.moments_matching(&MomentQuery {
            to_tick: Some(mid),
            ..MomentQuery::default()
        });
        let hi = file.moments_matching(&MomentQuery {
            from_tick: Some(mid + 1),
            ..MomentQuery::default()
        });
        assert_eq!([lo, hi].concat(), all, "seed {seed:#x}: windows partition");

        // Playing the whole file delivers exactly the extracted Moments, in order.
        let mut sim = load(&bytes);
        sim.step(sim.end_tick());
        let delivered: Vec<Moment> = events_of(&mut sim)
            .into_iter()
            .filter_map(|e| match e {
                ReplayEvent::Moment(m) => Some(m),
                _ => None,
            })
            .collect();
        assert_eq!(delivered, file.moments, "seed {seed:#x}");
    }
}
