// SPDX-License-Identifier: MPL-2.0
//! `.lsreplay` v1 over a real lockstep match (ADR-8513): the soak kit's host records a 4-seat
//! `LockSim` match over the impaired network in virtual time; the file reads back record for
//! record, re-writes to the same bytes, and the audit re-runs it bit-exact from its Fulls.

use regia_net::desync::ReplayRegistry;
use regia_net::lockstep::replay::{
    LsRecord, LsReplayReader, LsReplaySettings, LsReplayWriter, audit, audit_with,
};
use regia_net::lockstep::{LockstepConfig, LockstepFrame, SeatId, SeatInput};
use regia_net_soak::DEFAULT_SEED;
use regia_net_soak::lockstep::{
    InputMix, InputSource, LockReplay, LockSim, LockstepScenario, SoakHost, run_lockstep,
};
use regia_net_soak::profiles::Profile;

fn recorded_match() -> Vec<u8> {
    let profile = Profile::named("clean").expect("the clean profile");
    let mut s = LockstepScenario::new(profile, 4, DEFAULT_SEED, 12_000);
    s.host = SoakHost::Observer;
    s.config = LockstepConfig {
        hash_every: 30,
        ..LockstepConfig::default()
    };
    s.replay = Some(LsReplaySettings {
        record: true,
        hash_every_ticks: 10,
        ..LsReplaySettings::default()
    });
    let rec = run_lockstep(&s).expect("the run starts");
    assert!(rec.in_step && rec.verdicts.is_empty(), "a clean match");
    assert!(rec.frames_built >= 600, "{} frames", rec.frames_built);
    rec.replay.expect("the host recorded the match")
}

/// Acceptance (NL-02): a 600-tick 4-seat lockstep match round-trips: every frame from tick 0 in
/// order with all four seats, the Fulls at the tier's 2 Hz, the hashes every 10 ticks; decoding
/// and re-writing gives the same bytes; the audit steps every frame bit-exact.
#[test]
fn replay_round_trips_a_600_tick_lockstep_match() {
    let bytes = recorded_match();
    let mut reader = LsReplayReader::new(&bytes).expect("header");
    let header = reader.header().clone();
    assert_eq!(header.game_id, "regia.net_soak.lockstep");
    assert_eq!(header.seats, vec![0, 1, 2, 3]);
    assert_eq!(header.tick_rate, 60);
    assert_eq!(header.seed, DEFAULT_SEED);
    assert_eq!(header.full_every_ticks, 30, "2 Hz at 60 Hz");
    let mut records = Vec::new();
    while let Some(record) = reader.next_record().expect("record") {
        records.push(record);
    }
    let frames: Vec<u64> = records
        .iter()
        .filter_map(|r| match r {
            LsRecord::Inputs(i) => Some(i.frame.tick),
            _ => None,
        })
        .collect();
    assert!(frames.len() >= 600, "{} frames", frames.len());
    assert!(
        frames.iter().copied().eq(0..frames.len() as u64),
        "every tick from 0, once, in order"
    );
    assert!(records.iter().all(|r| match r {
        LsRecord::Inputs(i) => i.frame.inputs.len() == 4,
        _ => true,
    }));
    let fulls = records.iter().filter(|r| matches!(r, LsRecord::Full(_))).count();
    assert_eq!(fulls, 1 + frames.len() / 30, "the start and one every 30 ticks");
    let hashes = records
        .iter()
        .filter(|r| matches!(r, LsRecord::Hashes(_)))
        .count();
    assert_eq!(hashes, frames.len().div_ceil(10));

    // Re-written record for record: the same bytes.
    let mut writer = LsReplayWriter::new(Vec::new(), header, LsReplaySettings::default()).expect("header");
    for record in &records {
        writer.write_record(record).expect("record");
    }
    assert_eq!(writer.into_inner(), bytes);

    // The audit re-runs it bit-exact through the game's hook.
    let verdict = audit(&bytes, &mut LockReplay::default()).expect("audit");
    assert!(verdict.bit_exact(), "{:?}", verdict.summary());
    assert_eq!(verdict.frames, frames.len() as u64);
    assert_eq!(verdict.hashes_compared, hashes as u64);
    assert_eq!(verdict.fulls_compared, (fulls - 1) as u64);
    let mut registry = ReplayRegistry::new();
    registry.register("regia.net_soak.lockstep", || Box::new(LockReplay::default()));
    assert!(audit_with(&bytes, &registry).expect("registry").bit_exact());

    // One changed throttle of seat 1 (at the first tick from 333 with its input present) is
    // named at the next checkpoint, in `motion`.
    let mut flipped_at = None;
    let flipped = {
        let mut reader = LsReplayReader::new(&bytes).expect("header");
        let mut writer =
            LsReplayWriter::new(Vec::new(), reader.header().clone(), LsReplaySettings::default()).expect("h");
        while let Some(mut record) = reader.next_record().expect("record") {
            if let LsRecord::Inputs(i) = &mut record
                && i.frame.tick >= 333
                && flipped_at.is_none()
                && let Some(input) = i.frame.inputs[1].input.as_mut()
            {
                input.0[0] = input.0[0].wrapping_add(64);
                flipped_at = Some(i.frame.tick);
            }
            writer.write_record(&record).expect("record");
        }
        writer.into_inner()
    };
    let at = flipped_at.expect("seat 1 sent an input after tick 333");
    let checkpoint = at.next_multiple_of(10);
    let verdict = audit(&flipped, &mut LockReplay::default()).expect("audit");
    assert!(!verdict.bit_exact());
    assert_eq!(
        verdict.first_diverging_tick,
        Some(checkpoint),
        "{:?}",
        verdict.summary()
    );
    assert_eq!(verdict.last_agreed_tick, Some(checkpoint - 10));
    assert_eq!(verdict.subsystem.as_deref(), Some("motion"));
}

/// `LockSim`'s state codec round-trips every field bit for bit, and refuses short or doubled
/// states.
#[test]
fn lock_sim_state_round_trips() {
    let mut sim = LockSim::default();
    let mut inputs: Vec<InputSource> = (0..3).map(|s| InputSource::new(7, s, InputMix::ORDERS)).collect();
    for tick in 0..90 {
        let frame = LockstepFrame::<regia_net_soak::lockstep::LockInput> {
            tick,
            inputs: inputs
                .iter_mut()
                .enumerate()
                .map(|(s, i)| (SeatId(s as u16), SeatInput::Present(i.next_input())))
                .collect(),
            delay: 0,
            delay_from: 0,
            membership: if tick == 0 {
                (0..3)
                    .map(|s| regia_net::lockstep::MembershipChange {
                        seat: SeatId(s),
                        kind: regia_net::lockstep::MembershipKind::Join,
                    })
                    .collect()
            } else {
                Vec::new()
            },
            match_end: false,
        };
        sim.step(&frame);
    }
    let mut bytes = Vec::new();
    sim.save_state(&mut bytes);
    let back = LockSim::load_state(&bytes).expect("load");
    assert_eq!(back.hash(), sim.hash());
    assert_eq!(back, sim);
    assert!(LockSim::load_state(&bytes[..bytes.len() - 1]).is_err());
    assert!(LockSim::load_state(&bytes[..10]).is_err());
}
