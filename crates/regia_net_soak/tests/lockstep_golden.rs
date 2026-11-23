// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/tests/steering_golden.rs, commit c06c17084), ADR-8012.
// Portions copied from Direct Order 27 (ir-sim/src/desync_hash.rs, commit c06c17084), ADR-8012.
//! The cross-architecture pin of the lockstep driver (ADR-8146).
//!
//! A scripted 8-seat match runs 1,200 ticks through `LockstepHost` and `LockstepSeat` with no
//! network in between, the sample simulation steps every frame under the default float
//! environment, and the digest of the per-tick state hashes is pinned. The same number must come
//! out on x86-64 and aarch64, on Linux, Windows, macOS and Android: run this test on each
//! (`cargo test -p regia_net_soak --test lockstep_golden`).
//!
//! Direct Order 27's regia-absorb-3 steering golden rides the same driver (ADR-8307): its recorded
//! per-system words go to 8 seats with the golden's stick script as the seats' inputs, and the
//! words every seat steps must hash to the tag's SHA-256.

use std::sync::Arc;

use regia_net::lockstep::{
    HostRole, LockstepConfig, LockstepFrame, LockstepHost, LockstepSeat, SeatId, VerdictKind, Voter,
};
use regia_net_soak::lockstep::{InputMix, InputSource, LockGame, LockInput, LockSim, Track};
use regia_net_soak::vectors::{
    VectorDiff, VectorDrive, VectorRun, VectorText, VectorWorld, drive_vectors, first_divergence, sha256_hex,
};

const SEATS: u16 = 8;
const TICKS: u64 = 1_200;

/// FNV-1a over the per-tick hashes, as 64-bit little-endian words.
fn digest(hashes: impl Iterator<Item = u64>) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for word in hashes {
        for byte in word.to_le_bytes() {
            h ^= u64::from(byte);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    h
}

/// Plays the scripted match and returns each tick's hash as seat 0 stepped it, with the final
/// simulation.
fn play() -> (Vec<u64>, LockSim) {
    let (hashes, _, sim) = play_with_bodies();
    (hashes, sim)
}

/// [`play`], with the hash of seat 0's hulls alone after each tick.
fn play_with_bodies() -> (Vec<u64>, Vec<u64>, LockSim) {
    let report = regia_detsim::fpu::ensure_default_fp_env();
    assert!(report.was_default() || report.supported, "{report:?}");
    let config = LockstepConfig::default();
    let mut host = LockstepHost::<LockGame>::new(config.clone(), HostRole::Relay);
    let mut bodies: Vec<u64> = Vec::new();
    let mut seats: Vec<(LockstepSeat<LockGame>, InputSource, Track)> = (0..SEATS)
        .map(|i| {
            host.add_seat(SeatId(i));
            (
                LockstepSeat::new(config.clone(), SeatId(i)),
                InputSource::new(0xD027, i, InputMix::ACTION),
                Track::new(),
            )
        })
        .collect();
    host.start(0);
    for (seat, input, _) in &mut seats {
        seat.submit_input(&input.next_input()).expect("capture 0");
    }
    // One host tick per 17 virtual milliseconds: every payload is delivered at once.
    let mut now = 0u64;
    while host.tick() < TICKS {
        for (i, (seat, _, _)) in seats.iter_mut().enumerate() {
            for out in seat.outgoing(now) {
                host.receive(SeatId(i as u16), out.lane, &out.bytes, now)
                    .expect("seat payload");
            }
        }
        while host.poll_frame(now).is_some() {}
        for (to, out) in host.outgoing(now) {
            let (seat, input, track) = &mut seats[usize::from(to.0)];
            seat.receive(out.lane, &out.bytes, now).expect("host payload");
            while let Some(frame) = seat.next_frame() {
                let hash = track.step(&frame, config.hash_every);
                if to.0 == 0 {
                    bodies.push(track.sim.body_hash());
                }
                seat.report_hash(frame.tick, hash);
                seat.submit_input(&input.next_input())
                    .expect("one input per capture");
            }
        }
        now += 17;
    }
    // Every seat stepped the same thing.
    let reference: Vec<u64> = (0..TICKS).map(|t| seats[0].2.hashes[&t]).collect();
    for (_, _, track) in &seats {
        assert!(track.in_order);
        assert!((0..TICKS).all(|t| track.hashes[&t] == reference[t as usize]));
    }
    bodies.truncate(TICKS as usize);
    (reference, bodies, seats.swap_remove(0).2.sim)
}

/// The pinned digest. It moves only when the sample simulation, the input script or the frame
/// order changes on purpose; a difference between two hosts is a determinism bug.
///
/// First measured on aarch64 macOS (Apple M5, rustc 1.99.1) and on x86-64 Linux (rustc 1.99.0), the
/// same number on both (ADR-8146). Re-pinned for ADR-8876 (`MATH_BITS_VERSION` 1: the sample sim's bearings
/// call the canonical correctly rounded `regia_detsim::math::atan2`; was `0x475a_4037_8f18_e3b2`), on x86-64
/// Linux; the aarch64 macOS rerun is owed.
const GOLDEN: u64 = 0x8f6e_57a4_58ee_0774;

#[test]
fn the_lockstep_drivers_scripted_match_has_the_pinned_digest_on_this_host() {
    let (hashes, bodies, sim) = play_with_bodies();
    let got = digest(hashes.iter().copied());
    // The match did something: hulls moved on every tick, turned and hit each other. The state
    // hash cannot show that (it folds the tick, so it changes on a frozen simulation too): the
    // check is on the hulls alone.
    assert_eq!(sim.hulls.len(), usize::from(SEATS));
    assert!(sim.hits > 0, "no hull was ever hit: the script exercises nothing");
    assert_eq!(bodies.len(), TICKS as usize);
    let still = bodies.windows(2).filter(|w| w[0] == w[1]).count();
    assert_eq!(still, 0, "{still} ticks on which no hull moved");
    // Hulls spawn on a ring of 60 m; most of them ended somewhere else.
    let away = sim
        .hulls
        .values()
        .filter(|hull| {
            let from_center = (hull.x * hull.x + hull.y * hull.y).sqrt();
            (from_center - 60.0).abs() > 5.0
        })
        .count();
    assert!(
        away * 2 >= usize::from(SEATS),
        "{away} of {SEATS} hulls left the spawn ring"
    );
    assert_eq!(
        got,
        GOLDEN,
        "digest {got:#018x} on {} {}: every host must give {GOLDEN:#018x}",
        std::env::consts::ARCH,
        std::env::consts::OS
    );
}

#[test]
fn the_scripted_match_is_the_same_twice_in_one_process() {
    let (a, sim_a) = play();
    let (b, sim_b) = play();
    assert_eq!(a, b);
    assert_eq!(sim_a, sim_b);
}

/// Direct Order 27's steering golden, stepped by DO27's own simulation through the lockstep
/// driver. It cannot run in this repository (ADR-8307): the golden's 13 words a tick are folds of
/// DO27's whole game state (`ir_sim::desync_hash::system_hashes`: units, physics, weapons,
/// projectiles, economy, production, AI orders and commanders, pilots, buildings, terrain, match,
/// RNG and training), stepped by its `advance_tick` with AI combat, routing and a scripted death,
/// over a bootstrap of DO27's content. Reproducing the words means holding that game here, which
/// Regia does not absorb. DO27 runs this check in its own repository with
/// `regia_net_soak::vectors::drive_vectors` and its world; here the recorded words ride the same
/// driver (`do27_steering_golden_words_through_the_lockstep_driver_keep_their_sha`).
#[test]
#[ignore = "needs DO27's simulation (ADR-8307): the regia-absorb-3 steering golden folds DO27's whole game state per \
            system; DO27 runs it with regia_net_soak::vectors::drive_vectors and its own world"]
fn do27_steering_golden_through_the_lockstep_driver_matches_its_sha() {
    panic!(
        "not runnable here: the words fold DO27's GameplayRuntimeState (ADR-8307); \
         see do27_steering_golden_words_through_the_lockstep_driver_keep_their_sha"
    );
}

/// The regia-absorb-3 steering golden (DO27 tag regia-absorb-3, commit
/// c06c17084218b7bd2fa80b5a15492d27483069d9, the `steering-1200.txt` fixture under `spec/fixtures/`),
/// byte for byte. Its line ends must stay `\n` (`.gitattributes`).
const DO27_STEERING: &str = include_str!("fixtures/do27/steering-1200.txt");

/// The SHA-256 the regia-absorb-3 tag note gives for the golden. Never edit it to match a run.
const DO27_STEERING_SHA256: &str = "535ce827795375e623e64317e41ddfa118a5d0eb33c956d1b001c52fa235dbb5";

/// The golden's system order (DO27 `SYSTEM_NAMES`).
const DO27_SYSTEMS: [&str; 13] = [
    "units",
    "physics",
    "weapons",
    "projectiles",
    "economy",
    "production",
    "ai",
    "pilots",
    "buildings",
    "terrain",
    "match",
    "rng",
    "training",
];

/// DO27 ticks in the golden.
const DO27_TICKS: usize = 1_200;

/// The stick script of DO27's two driven hulls (ids 100 and 101, seats 0 and 1) at golden tick
/// `tick`, as the regia-absorb-3 harness feeds it: throttle 0.55, a steer and a strafe that change
/// every 73 ticks, a jump press every 157 ticks, a shot every 11 ticks and an aim pitch per phase.
/// The floats ride bit for bit in the command blob (`throttle strafe steer pitch`, `f32`
/// little-endian); bit 0 of `buttons` fires, bit 1 jumps. Seats 2 to 7 (the six AI hulls, ids 102
/// to 107) send an empty input: the simulation's AI drives them.
fn do27_stick(seat: SeatId, tick: u64) -> LockInput {
    if seat.0 >= 2 {
        return LockInput::default();
    }
    let id = 100 + u64::from(seat.0);
    let phase = ((tick / 73 + id) % 4) as usize;
    let steer = [0.73f32, -0.41, -0.89, 0.27][phase];
    let strafe = [0.61f32, -0.83, 0.39, -0.57][phase];
    let pitch = (phase as f32 - 1.5) * 0.09;
    let mut command = Vec::with_capacity(16);
    for v in [0.55f32, strafe, steer, pitch] {
        command.extend_from_slice(&v.to_le_bytes());
    }
    LockInput {
        buttons: u16::from(tick.is_multiple_of(11)) | (u16::from(tick.is_multiple_of(157)) << 1),
        command,
        ..LockInput::default()
    }
}

/// A seat's world that replays the recorded golden: it checks that each frame carries every
/// seat's scripted input for the next golden tick and answers with that tick's 13 words. Frames
/// with no input at all (the input delay at the start) step nothing. `plant` flips one bit of one
/// word at one tick (a desync of that system), on this seat only.
#[derive(Clone)]
struct RecordedSteering {
    rows: Arc<Vec<Vec<u64>>>,
    next: usize,
    plant: Option<(usize, usize)>,
}

impl VectorWorld for RecordedSteering {
    fn step(&mut self, frame: &LockstepFrame<LockInput>) -> Option<Vec<u64>> {
        let present = frame.inputs.iter().filter(|(_, i)| i.as_ref().is_some()).count();
        if present == 0 {
            return None;
        }
        assert_eq!(
            present,
            usize::from(SEATS),
            "frame {}: a seat's input is missing",
            frame.tick
        );
        for (seat, input) in &frame.inputs {
            let want = do27_stick(*seat, self.next as u64);
            assert_eq!(
                input.as_ref(),
                Some(&want),
                "frame {}, golden tick {}, seat {}: the frame does not carry the scripted input",
                frame.tick,
                self.next,
                seat.0
            );
        }
        let mut row = self.rows.get(self.next)?.clone();
        if let Some((tick, system)) = self.plant
            && tick == self.next
        {
            row[system] ^= 1 << 17;
        }
        self.next += 1;
        Some(row)
    }
}

fn do27_golden() -> VectorText {
    VectorText::parse(DO27_STEERING).expect("the copied golden parses")
}

fn drive_do27(plant: Option<(u16, usize, usize)>) -> VectorRun {
    let golden = Arc::new(do27_golden().rows);
    let drive = VectorDrive::fixed_delay(SEATS, DO27_TICKS);
    drive_vectors(&drive, do27_stick, |seat| RecordedSteering {
        rows: Arc::clone(&golden),
        next: 0,
        plant: plant.and_then(|(s, tick, system)| (s == seat.0).then_some((tick, system))),
    })
    .expect("no payload refused")
}

/// The copied golden is regia-absorb-3's, its 1,200 rows of 13 words ride `LockstepHost` and
/// `LockstepSeat` to 8 seats with each seat's scripted stick input on the right tick, every seat
/// ends with the same rows, no checkpoint vote differs, and the rows written back in the golden's
/// text form hash to the tag's SHA-256.
#[test]
fn do27_steering_golden_words_through_the_lockstep_driver_keep_their_sha() {
    assert_eq!(
        sha256_hex(DO27_STEERING.as_bytes()),
        DO27_STEERING_SHA256,
        "the copied golden is not regia-absorb-3's (a line-end conversion?)"
    );
    let golden = do27_golden();
    assert_eq!(golden.rows.len(), DO27_TICKS);
    assert!(golden.rows.iter().all(|r| r.len() == DO27_SYSTEMS.len()));
    assert!(
        golden
            .header
            .ends_with(&format!("tick {}", DO27_SYSTEMS.join(" "))),
        "{}",
        golden.header
    );

    let run = drive_do27(None);
    assert!(run.in_step(DO27_TICKS), "seats out of step");
    assert!(run.verdicts.is_empty(), "{:?}", run.verdicts);
    assert!(run.removed.is_empty());
    for seat in &run.seats {
        assert_eq!(seat.skipped, 0, "seat {}: a capture lost its tick", seat.seat.0);
        assert_eq!(
            first_divergence(&golden.rows, &seat.rows),
            None,
            "seat {}",
            seat.seat.0
        );
    }
    let words = VectorText {
        header: golden.header.clone(),
        rows: run.seats[0].rows.clone(),
    }
    .encode();
    assert_eq!(words, DO27_STEERING);
    assert_eq!(
        sha256_hex(words.as_bytes()),
        DO27_STEERING_SHA256,
        "on {} {}",
        std::env::consts::ARCH,
        std::env::consts::OS
    );
}

/// One seat's `ai` word drifts at golden tick 777: the quorum names that seat at the next
/// checkpoint, the host's probe finds the first frame whose hash differs (frame 781: tick 777
/// after the 4-tick input delay), the self-check replay reproduces it, the per-system probe names
/// the tick and the system, and the state transfer from the majority leaves every seat on the
/// golden's rows and SHA.
#[test]
fn a_drifted_system_word_is_named_with_its_tick_and_repaired_through_the_driver() {
    let ai = DO27_SYSTEMS.iter().position(|s| *s == "ai").expect("ai");
    let run = drive_do27(Some((3, 777, ai)));
    let first = run.verdicts.first().expect("the drift is voted on");
    assert_eq!(first.tick, 810, "the first checkpoint after frame 781");
    assert_eq!(first.last_agreed, Some(780));
    assert_eq!(first.kind, VerdictKind::Diverged);
    assert_eq!(first.diverged, [Voter::Seat(SeatId(3))]);
    assert!(
        run.located.contains(&(810, Voter::Seat(SeatId(3)), 781)),
        "{:?}",
        run.located
    );
    assert_eq!(run.seats[3].self_checks, 1);
    assert_eq!(run.seats[3].resyncs, 1);
    assert!(
        run.seats
            .iter()
            .enumerate()
            .all(|(i, s)| i == 3 || s.resyncs == 0)
    );
    assert_eq!(run.probes.len(), 1, "{:?}", run.probes);
    assert_eq!(run.probes[0].seat, SeatId(3));
    assert_eq!(
        run.probes[0].diff,
        Some(VectorDiff {
            row: 777,
            system: Some(ai)
        })
    );
    assert!(run.removed.is_empty());
    assert!(run.in_step(DO27_TICKS), "the transfer did not bring seat 3 back");
    let golden = do27_golden();
    let words = VectorText {
        header: golden.header,
        rows: run.seats[3].rows.clone(),
    }
    .encode();
    assert_eq!(sha256_hex(words.as_bytes()), DO27_STEERING_SHA256);
}
