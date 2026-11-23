// SPDX-License-Identifier: MPL-2.0
//! Loudness conformance on the published synthetic test signals (`docs/specs/audio-loudness.md`
//! § 2 and § 7; spec-43 audit, ADR-5662).
//!
//! EBU Tech 3341 (loudness metering, "EBU Mode") and EBU Tech 3342 (loudness range) define their
//! minimum meter requirements with stereo 1 kHz sines built from level steps. The signals here are
//! built the same way from their published descriptions (level in dBFS peak, duration in seconds),
//! and the expected values and tolerances are the ones a conforming meter must show (±0.1 LU for
//! loudness, ±1 LU for the range), not values read back from this meter.

use regia_audio::{LoudnessMeter, measure_loudness};

const RATE: u32 = 48_000;

/// A 1 kHz sine on two in-phase channels, interleaved, in level steps of `(dBFS, seconds)`. The
/// phase runs on across steps, so a step changes only the amplitude.
fn steps(levels: &[(f64, f64)]) -> Vec<f32> {
    let mut out = Vec::new();
    let mut frame = 0u64;
    for &(db, seconds) in levels {
        let amp = 10f64.powf(db / 20.0);
        let frames = (seconds * f64::from(RATE)).round() as u64;
        for _ in 0..frames {
            let t = frame as f64 / f64::from(RATE);
            let s = (amp * (std::f64::consts::TAU * 1000.0 * t).sin()) as f32;
            out.extend([s, s]);
            frame += 1;
        }
    }
    out
}

fn assert_near(what: &str, value: f32, want: f32, tolerance: f32) {
    assert!(
        (value - want).abs() <= tolerance,
        "{what}: read {value}, want {want} ± {tolerance}"
    );
}

/// Tech 3341 cases 1 and 2: a steady tone reads its level as momentary, short-term and integrated
/// loudness (a −23 dBFS tone on two in-phase channels is −23 LUFS).
#[test]
fn a_steady_tone_reads_its_level_on_every_time_scale() {
    for level in [-23.0f64, -33.0] {
        let loud = measure_loudness(&steps(&[(level, 20.0)]), 2, RATE).expect("valid");
        let want = level as f32;
        assert_near("momentary", loud.momentary_lufs, want, 0.1);
        assert_near("short-term", loud.short_term_lufs, want, 0.1);
        assert_near("integrated", loud.integrated_lufs, want, 0.1);
    }
}

/// Tech 3341 case 3: quiet passages 13 LU under the programme fall to the relative gate.
#[test]
fn the_relative_gate_drops_passages_more_than_10_lu_down() {
    let signal = steps(&[(-36.0, 10.0), (-23.0, 60.0), (-36.0, 10.0)]);
    let loud = measure_loudness(&signal, 2, RATE).expect("valid");
    assert_near("integrated", loud.integrated_lufs, -23.0, 0.1);
}

/// Tech 3341 case 4: near-silence under −70 LUFS falls to the absolute gate, the quiet passages
/// to the relative gate.
#[test]
fn the_absolute_and_relative_gates_work_together() {
    let signal = steps(&[
        (-72.0, 10.0),
        (-36.0, 10.0),
        (-23.0, 60.0),
        (-36.0, 10.0),
        (-72.0, 10.0),
    ]);
    let loud = measure_loudness(&signal, 2, RATE).expect("valid");
    assert_near("integrated", loud.integrated_lufs, -23.0, 0.1);
}

/// Tech 3341 case 5: passages 6 LU apart are all inside the relative gate, so the integrated value
/// is their power mean (−26 and −20 LUFS for about a third and a third: −23 LUFS).
#[test]
fn passages_inside_the_relative_gate_average_in_the_power_domain() {
    let signal = steps(&[(-26.0, 20.0), (-20.0, 20.1), (-26.0, 20.0)]);
    let loud = measure_loudness(&signal, 2, RATE).expect("valid");
    assert_near("integrated", loud.integrated_lufs, -23.0, 0.1);
}

/// Tech 3342 cases 1 to 4: the loudness range of level steps is the step between the 10th and 95th
/// percentile levels, with readings more than 20 LU under the power mean left out (case 4's
/// −50 dBFS passages).
#[test]
fn the_loudness_range_of_level_steps_follows_tech_3342() {
    let cases: [(&[(f64, f64)], f32); 4] = [
        (&[(-20.0, 20.0), (-30.0, 20.0)], 10.0),
        (&[(-20.0, 20.0), (-15.0, 20.0)], 5.0),
        (&[(-40.0, 20.0), (-20.0, 20.0)], 20.0),
        (
            &[
                (-50.0, 20.0),
                (-35.0, 20.0),
                (-20.0, 20.0),
                (-35.0, 20.0),
                (-50.0, 20.0),
            ],
            15.0,
        ),
    ];
    for (levels, want) in cases {
        let loud = measure_loudness(&steps(levels), 2, RATE).expect("valid");
        assert_near(&format!("LRA of {levels:?}"), loud.loudness_range_lu, want, 1.0);
    }
}

/// § 3: a non-finite sample is dropped and the K-weighting cleared, so later finite samples read
/// exactly as they would on a fresh meter. At the start of a stream a NaN is the same as a zero.
#[test]
fn a_non_finite_sample_reads_as_silence_and_the_meter_recovers() {
    let tone = steps(&[(-23.0, 4.0)]);
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let mut poisoned = LoudnessMeter::new(2, RATE).expect("valid");
        poisoned.push(&[bad, bad]);
        poisoned.push(&tone);
        let mut clean = LoudnessMeter::new(2, RATE).expect("valid");
        clean.push(&[0.0, 0.0]);
        clean.push(&tone);
        assert_eq!(poisoned.finish(), clean.finish(), "{bad} at the start");
    }

    // Mid-stream: after the bad sample the filters restart from rest, so once the 400 ms window
    // holds only the new tone the momentary reading equals a fresh meter's on that tone.
    let loud = steps(&[(-10.0, 2.0)]);
    let mut poisoned = LoudnessMeter::new(2, RATE).expect("valid");
    poisoned.push(&loud);
    poisoned.push(&[f32::NAN, 0.0]);
    poisoned.push(&tone);
    let mut fresh = LoudnessMeter::new(2, RATE).expect("valid");
    fresh.push(&[0.0, 0.0]);
    fresh.push(&tone);
    assert_near(
        "momentary after recovery",
        poisoned.momentary_lufs(),
        fresh.momentary_lufs(),
        1e-3,
    );
    let result = poisoned.finish();
    assert!(result.integrated_lufs.is_finite(), "{result:?}");
    assert!(result.loudness_range_lu.is_finite(), "{result:?}");
    assert!(result.true_peak_db.is_finite(), "{result:?}");
}

/// § 3 (as built, ADR-5662): with no signal the loudness values are `−∞`, while the true peak sits
/// at the `−100` dBTP floor of `gain_to_db`, the same before any sample, on silence, and after
/// `finish`.
#[test]
fn an_empty_meter_reads_minus_infinity_loudness_and_the_true_peak_floor() {
    let meter = LoudnessMeter::new(1, RATE).expect("valid");
    assert_eq!(meter.momentary_lufs(), f32::NEG_INFINITY);
    assert_eq!(meter.short_term_lufs(), f32::NEG_INFINITY);
    assert_eq!(meter.true_peak_db(), -100.0);
    let empty = meter.finish();
    assert_eq!(empty.integrated_lufs, f32::NEG_INFINITY);
    assert_eq!(empty.momentary_lufs, f32::NEG_INFINITY);
    assert_eq!(empty.short_term_lufs, f32::NEG_INFINITY);
    assert_eq!(empty.loudness_range_lu, 0.0);
    assert_eq!(empty.true_peak_db, -100.0);
}
