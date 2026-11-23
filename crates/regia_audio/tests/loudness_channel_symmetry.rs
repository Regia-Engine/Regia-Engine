// SPDX-License-Identifier: MPL-2.0
//! Stereo loudness contracts: channel identity and polarity cannot change power-based results.

use regia_audio::{Loudness, LoudnessMeter, measure_loudness};

const RATE: usize = 48_000;
const TOLERANCE: f32 = 1.0e-4;

fn asymmetric_stereo(frames: usize, offset: usize) -> Vec<f32> {
    (offset..offset + frames)
        .flat_map(|frame| {
            let t = frame as f64 / RATE as f64;
            let envelope = match frame / RATE {
                0 => 0.08,
                1 => 0.35,
                2 => 0.015,
                3 => 0.55,
                4 => 0.0,
                _ => 0.12,
            };
            let left = envelope * (std::f64::consts::TAU * 997.0 * t).sin();
            let right = (envelope * 0.7) * (std::f64::consts::TAU * 1_403.0 * t + 0.31).sin();
            let impulse = if frame == RATE * 3 + 127 { 0.9 } else { 0.0 };
            [(left + impulse) as f32, (right - impulse * 0.4) as f32]
        })
        .collect()
}

fn swapped(samples: &[f32]) -> Vec<f32> {
    samples
        .as_chunks::<2>()
        .0
        .iter()
        .flat_map(|frame| [frame[1], frame[0]])
        .collect()
}

fn invert_left(samples: &[f32]) -> Vec<f32> {
    samples
        .as_chunks::<2>()
        .0
        .iter()
        .flat_map(|frame| [-frame[0], frame[1]])
        .collect()
}

fn invert_both(samples: &[f32]) -> Vec<f32> {
    samples.iter().map(|sample| -*sample).collect()
}

fn assert_loudness_eq(actual: Loudness, expected: Loudness, context: &str) {
    for (name, actual, expected) in [
        ("momentary", actual.momentary_lufs, expected.momentary_lufs),
        ("short-term", actual.short_term_lufs, expected.short_term_lufs),
        ("integrated", actual.integrated_lufs, expected.integrated_lufs),
        ("range", actual.loudness_range_lu, expected.loudness_range_lu),
        ("true peak", actual.true_peak_db, expected.true_peak_db),
    ] {
        if expected == f32::NEG_INFINITY {
            assert_eq!(actual, expected, "{context}: {name}");
        } else {
            assert!(
                (actual - expected).abs() <= TOLERANCE,
                "{context}: {name} was {actual}, expected {expected}"
            );
        }
    }
}

fn measure(samples: &[f32]) -> Loudness {
    measure_loudness(samples, 2, RATE as u32).expect("nonempty stereo at supported rate")
}

#[test]
fn stereo_power_results_are_invariant_to_channel_order_and_polarity() {
    let samples = asymmetric_stereo(RATE * 6, 0);
    let expected = measure(&samples);
    assert_loudness_eq(measure(&swapped(&samples)), expected, "L/R swap");
    assert_loudness_eq(measure(&invert_left(&samples)), expected, "left polarity");
    assert_loudness_eq(measure(&invert_both(&samples)), expected, "both polarities");

    let silence = vec![0.0; RATE * 2];
    let silent = measure(&silence);
    assert_eq!(silent.integrated_lufs, f32::NEG_INFINITY);
    assert_loudness_eq(measure(&swapped(&silence)), silent, "silent L/R swap");
    assert_loudness_eq(measure(&invert_both(&silence)), silent, "silent polarity");
}

#[test]
fn a_single_active_channel_has_the_same_result_on_left_or_right() {
    let mono: Vec<f32> = (0..RATE * 3)
        .map(|frame| (0.2 * (std::f64::consts::TAU * 997.0 * frame as f64 / RATE as f64).sin()) as f32)
        .collect();
    let left: Vec<f32> = mono.iter().flat_map(|&sample| [sample, 0.0]).collect();
    let right: Vec<f32> = mono.iter().flat_map(|&sample| [0.0, sample]).collect();
    let left_result = measure(&left);
    assert_loudness_eq(measure(&right), left_result, "one active channel moved right");

    let mono_result = measure_loudness(&mono, 1, RATE as u32).expect("nonempty mono");
    assert!(
        (left_result.integrated_lufs - mono_result.integrated_lufs).abs() < TOLERANCE,
        "stereo one-channel power should match mono: {} vs {}",
        left_result.integrated_lufs,
        mono_result.integrated_lufs
    );
}

#[test]
fn finish_reads_are_observational_before_later_frames_arrive() {
    let prefix = asymmetric_stereo(RATE * 2 + 137, 0);
    let suffix = asymmetric_stereo(RATE * 3 + 91, RATE * 2 + 137);
    let mut continued = LoudnessMeter::new(2, RATE as u32).expect("supported stereo meter");
    let mut uninterrupted = LoudnessMeter::new(2, RATE as u32).expect("supported stereo meter");
    continued.push(&prefix);
    uninterrupted.push(&prefix);
    let first_read = continued.finish();
    assert_loudness_eq(continued.finish(), first_read, "repeated finish");
    continued.push(&suffix);
    uninterrupted.push(&suffix);
    assert_loudness_eq(continued.finish(), uninterrupted.finish(), "push after finish");
}
