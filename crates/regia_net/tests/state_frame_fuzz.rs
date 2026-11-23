// SPDX-License-Identifier: MPL-2.0
//! ADR-8761: the client's state-frame apply path (`client_apply_state_frame` and the record
//! decoders under it) under seeded mutations of recorded frames. Fixed seeds, fixed iteration
//! counts, no wall clock: a failure names the seed, the iteration and the input.

use regia_net::testing::state_frame_fuzz::{
    Report, apply, apply_after, apply_after_counted, recorded, run, scratch_world, with_extra_record,
};

fn run_or_report(seed: u64, iterations: usize) -> Report {
    match run(seed, iterations) {
        Ok(report) => report,
        Err(f) => panic!(
            "seed {seed:#x} iteration {} (recorded frame {}): {}\ninput: {:02x?}",
            f.iteration, f.frame, f.message, f.input
        ),
    }
}

/// The recording covers the paths the fuzz is for: several frames, spawns and deltas.
#[test]
fn the_recording_is_deterministic_and_applies_cleanly() {
    let frames = recorded();
    assert!(frames.len() >= 20, "{} frames", frames.len());
    assert_eq!(
        regia_net::testing::state_frame_fuzz::record(),
        frames,
        "the same session records the same bytes"
    );
    let (last, prefix) = frames.split_last().expect("frames");
    assert_eq!(apply_after(prefix, last, &[]), Ok(true));
    // Applied in order, the recording spawns and updates copies with no error.
    let (applied, c) =
        regia_net::testing::state_frame_fuzz::apply_after_counted(&frames[..1], &frames[1], &frames[2..])
            .expect("bounds");
    assert!(applied);
    assert!(c.applied >= 20, "{c:?}");
    assert_eq!((c.record_errors, c.baseline_missing), (0, 0), "{c:?}");
}

/// A bounded run: 1500 mutations under one fixed seed, no panic and no broken bound.
#[test]
fn mutated_state_frames_never_panic_the_client() {
    let report = run_or_report(0x57a7_e0f2, 1500);
    assert_eq!(report.applied + report.refused, 1500);
    assert!(report.applied > 0 && report.refused > 0, "{report:?}");
    // The mutations reach the record decoders: records still apply, and damaged ones are caught
    // one by one rather than by the frame's framing.
    assert!(
        report.records_applied > 0 && report.record_errors > 0,
        "{report:?}"
    );
    eprintln!("state frame fuzz: {report:?}");
}

/// A second seed, so a fix for one input is not tuned to one stream.
#[test]
fn mutated_state_frames_never_panic_the_client_second_seed() {
    run_or_report(0x0dd_ba11, 1500);
}

/// ADR-8761, found by `regia_agent`'s registry (`net.state_frame_apply: hostile leading word was
/// accepted`): a frame whose tag is not `STATE` (7) or `STATE_ENTROPY` (8) was applied as a state
/// frame. The runtime routes only those tags to the apply path, which now checks it too.
#[test]
fn a_frame_of_another_tag_is_not_a_state_frame() {
    let frames = recorded();
    let fixture = frames.iter().max_by_key(|f| f.len()).expect("frames");
    for tag in [0u8, 1, 4, 6, 9, 0xff] {
        let mut other = fixture.clone();
        other[0] = tag;
        let mut world = scratch_world();
        assert!(
            apply(&mut world, &other).is_err(),
            "tag {tag} applied as a state frame"
        );
    }
    let mut hostile = fixture.clone();
    hostile[..4].fill(0xff);
    assert!(apply(&mut scratch_world(), &hostile).is_err());
    assert!(
        apply(&mut scratch_world(), fixture).is_ok(),
        "the fixture itself applies"
    );
}

/// ADR-8850: the scratch world carries the smallest cap the recording fits (four ids get spawn
/// records: three live ones plus the copy whose despawn notice travels on the reliable tick
/// frame, never in a `STATE` frame, so its ring stays), so the cap refusal path of ADR-8760 and
/// the "maps <= cap" bound of `apply_after_counted` are reachable (rev-claude-55 finding 6: at
/// the default 65536 the bound was vacuous). A further fresh id in a spawn record appended to a
/// recorded frame refuses that frame whole instead of growing the maps.
#[test]
fn a_fourth_fresh_id_past_the_scratch_cap_is_refused() {
    let frames = recorded();
    let cap = scratch_world()
        .resource::<regia_net::replication::ClientReplication>()
        .state
        .cap();
    assert_eq!(cap, 4, "the scratch cap is what the recording holds");
    let (last, prefix) = frames.split_last().expect("frames");
    let mutated = with_extra_record(last, frames.first().expect("frames"), 4095).expect("re-encoded frame");
    let (applied, c) = apply_after_counted(prefix, &mutated, &[]).expect("no bound was broken on the way");
    assert!(!applied, "the frame with a further fresh id is refused");
    assert!(c.over_cap >= 1, "the refusal is counted: {c:?}");
}
