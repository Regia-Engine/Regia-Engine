// SPDX-License-Identifier: MPL-2.0
//! Streaming loudness results must depend on the complete frame sequence, not push boundaries.

use regia_audio::{Loudness, LoudnessMeter};

fn pcm(channels: usize, rate: u32, frames: usize, seed: usize) -> Vec<f32> {
    (0..frames)
        .flat_map(|frame| {
            (0..channels).map(move |channel| {
                let n = frame + seed;
                let phase = n as f64 * (std::f64::consts::TAU * 997.0 / f64::from(rate));
                (phase.sin() * (0.12 + channel as f64 * 0.03)) as f32
            })
        })
        .collect()
}

fn meter(channels: u8, rate: u32, samples: &[f32], chunk_frames: &[usize]) -> Loudness {
    let mut m = LoudnessMeter::new(channels, rate).expect("supported meter configuration");
    let channels = usize::from(channels);
    let mut at = 0;
    let mut i = 0;
    while at < samples.len() / channels {
        let count = chunk_frames[i % chunk_frames.len()].min(samples.len() / channels - at);
        let end = at + count;
        m.push(&samples[at * channels..end * channels]);
        at = end;
        i += 1;
    }
    m.finish()
}

fn assert_same(actual: Loudness, expected: Loudness, context: &str) {
    for (name, a, e) in [
        ("momentary", actual.momentary_lufs, expected.momentary_lufs),
        ("short-term", actual.short_term_lufs, expected.short_term_lufs),
        ("integrated", actual.integrated_lufs, expected.integrated_lufs),
        ("range", actual.loudness_range_lu, expected.loudness_range_lu),
        ("true peak", actual.true_peak_db, expected.true_peak_db),
    ] {
        if e.is_infinite() {
            assert_eq!(a, e, "{context}: {name}");
        } else {
            assert!((a - e).abs() <= 1.0e-5, "{context}: {name} {a} != {e}");
        }
    }
}

#[test]
fn frame_aligned_partitions_preserve_results_across_window_boundaries() {
    for channels in [1u8, 2] {
        for rate in [44_100u32, 48_000] {
            for frames in [
                (rate as usize / 10) - 1,
                rate as usize / 10,
                (rate as usize / 10) + 1,
                (rate as usize * 4 / 10) - 1,
                rate as usize * 4 / 10,
                (rate as usize * 4 / 10) + 1,
                rate as usize * 4,
            ] {
                let samples = pcm(usize::from(channels), rate, frames, 0);
                let expected = meter(channels, rate, &samples, &[frames.max(1)]);
                assert_same(meter(channels, rate, &samples, &[1]), expected, "single frames");
                assert_same(
                    meter(channels, rate, &samples, &[7, 113, 3, 997, 41]),
                    expected,
                    "irregular chunks",
                );
            }
        }
    }
}

#[test]
fn silence_tone_silence_and_non_finite_boundaries_are_partition_invariant() {
    for channels in [1u8, 2] {
        for rate in [44_100u32, 48_000] {
            let ch = usize::from(channels);
            let hop = rate as usize / 10;
            let gate = rate as usize * 4 / 10;
            let segments = [
                (0.0, hop - 1),
                (0.12, hop + 1),
                (0.0, gate - 1),
                (0.12, gate + 1),
                (0.0, rate as usize / 2),
                (0.18, rate as usize * 3),
                (0.0, rate as usize / 2),
            ];
            let mut samples = Vec::new();
            let mut frame_offset = 0;
            for (amplitude, frames) in segments {
                let mut segment = pcm(ch, rate, frames, frame_offset);
                for sample in &mut segment {
                    *sample *= amplitude as f32 / 0.12;
                }
                samples.extend(segment);
                frame_offset += frames;
            }
            // Put non-finite samples on both sides of irregular chunk boundaries.
            let mut boundary = 0;
            for count in [37, 2, 509, 11, 1001] {
                boundary += count;
                if boundary < frame_offset {
                    samples[(boundary - 1) * ch] = f32::INFINITY;
                    samples[boundary * ch] = f32::NAN;
                }
            }
            let expected = meter(channels, rate, &samples, &[samples.len() / ch]);
            assert!(expected.short_term_lufs.is_finite());
            assert!(expected.integrated_lufs.is_finite());
            assert!(expected.loudness_range_lu.is_finite());
            assert!(expected.true_peak_db.is_finite());
            assert_same(meter(channels, rate, &samples, &[1]), expected, "single frames");
            assert_same(
                meter(channels, rate, &samples, &[37, 2, 509, 11, 1001]),
                expected,
                "irregular chunks",
            );
        }
    }
}

#[test]
fn each_push_ignores_its_own_trailing_partial_frame() {
    let complete = [0.2f32, -0.2, 0.1, -0.1, 0.3, -0.3];
    let mut control = LoudnessMeter::new(2, 48_000).expect("stereo");
    control.push(&complete);

    let mut partitioned = LoudnessMeter::new(2, 48_000).expect("stereo");
    partitioned.push(&[0.2, -0.2, 99.0]);
    partitioned.push(&[0.1, -0.1, 88.0]);
    partitioned.push(&[0.3, -0.3, 77.0]);
    assert_same(
        partitioned.finish(),
        control.finish(),
        "per-push trailing samples ignored",
    );
}

#[test]
fn finish_is_repeatable_and_later_pushes_match_an_uninterrupted_meter() {
    for channels in [1u8, 2] {
        let ch = usize::from(channels);
        let prefix = pcm(ch, 48_000, 19_201, 0);
        let suffix = pcm(ch, 48_000, 20_003, 19_201);
        let mut continued = LoudnessMeter::new(channels, 48_000).expect("supported");
        let mut uninterrupted = LoudnessMeter::new(channels, 48_000).expect("supported");
        continued.push(&prefix);
        uninterrupted.push(&prefix);
        assert_same(continued.finish(), continued.finish(), "repeat finish");
        continued.push(&suffix);
        uninterrupted.push(&suffix);
        assert_same(continued.finish(), uninterrupted.finish(), "push after finish");
    }
}

#[test]
fn exact_silence_keeps_negative_infinity_classification() {
    for channels in [1u8, 2] {
        for rate in [44_100u32, 48_000] {
            let samples = vec![0.0f32; rate as usize * usize::from(channels)];
            let output = meter(channels, rate, &samples, &[1, 13, 409]);
            assert_eq!(output.momentary_lufs, f32::NEG_INFINITY);
            assert_eq!(output.short_term_lufs, f32::NEG_INFINITY);
            assert_eq!(output.integrated_lufs, f32::NEG_INFINITY);
            assert_eq!(output.loudness_range_lu, 0.0);
        }
    }
}
