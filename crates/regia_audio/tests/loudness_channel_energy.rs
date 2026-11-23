// SPDX-License-Identifier: MPL-2.0
//! Stereo loudness uses independent channel power and peak streams (BS.1770).

use regia_audio::{Loudness, LoudnessMeter, measure_loudness};

const RATES: [u32; 3] = [44_100, 48_000, 96_000];

fn tone(rate: u32, hz: f64, amplitude: f64, phase: f64, frames: usize) -> Vec<f32> {
    (0..frames)
        .map(|frame| {
            (amplitude * (std::f64::consts::TAU * hz * frame as f64 / f64::from(rate) + phase).sin()) as f32
        })
        .collect()
}

fn stereo(left: &[f32], right: &[f32]) -> Vec<f32> {
    left.iter()
        .zip(right)
        .flat_map(|(&left, &right)| [left, right])
        .collect()
}

fn transform_channels(samples: &[f32], swap: bool, invert_left: bool, invert_right: bool) -> Vec<f32> {
    let (frames, remainder) = samples.as_chunks::<2>();
    assert!(remainder.is_empty(), "interleaved stereo frame must be complete");
    frames
        .iter()
        .flat_map(|frame| {
            let [left, right] = if swap {
                [frame[1], frame[0]]
            } else {
                [frame[0], frame[1]]
            };
            [
                if invert_left { -left } else { left },
                if invert_right { -right } else { right },
            ]
        })
        .collect()
}

fn assert_same_stats(actual: Loudness, expected: Loudness, tolerance: f32, label: &str) {
    for (name, a, b) in [
        ("momentary", actual.momentary_lufs, expected.momentary_lufs),
        ("short-term", actual.short_term_lufs, expected.short_term_lufs),
        ("integrated", actual.integrated_lufs, expected.integrated_lufs),
        ("LRA", actual.loudness_range_lu, expected.loudness_range_lu),
        ("true peak", actual.true_peak_db, expected.true_peak_db),
    ] {
        assert!(a.is_finite() && b.is_finite(), "{label}: {name}: {a} vs {b}");
        assert!((a - b).abs() <= tolerance, "{label}: {name}: {a} vs {b}");
    }
}

fn measure(samples: &[f32], rate: u32) -> Loudness {
    measure_loudness(samples, 2, rate).expect("valid stereo PCM")
}

#[test]
fn channel_exchange_and_polarity_inversion_preserve_stereo_statistics() {
    for rate in RATES {
        let frames = rate as usize * 7;
        let left = tone(rate, 997.0, 0.18, 0.31, frames);
        let right = tone(rate, 997.0, 0.11, 1.17, frames);
        let original = stereo(&left, &right);
        let reference = measure(&original, rate);
        for (swap, invert_left, invert_right, label) in [
            (true, false, false, "exchange"),
            (false, true, false, "invert left"),
            (false, false, true, "invert right"),
            (false, true, true, "invert both"),
            (true, true, false, "exchange and invert left"),
        ] {
            let changed = transform_channels(&original, swap, invert_left, invert_right);
            assert_same_stats(measure(&changed, rate), reference, 0.015, label);
        }
    }
}

#[test]
fn anti_phase_channels_do_not_cancel_loudness_or_channel_peak() {
    for rate in RATES {
        let frames = rate as usize * 7;
        let left = tone(rate, 997.0, 0.2, 0.43, frames);
        let right: Vec<f32> = left.iter().map(|sample| -*sample).collect();
        let anti_phase = stereo(&left, &right);
        let single_channel = measure_loudness(&left, 1, rate).expect("valid mono PCM");
        let anti_phase_result = measure(&anti_phase, rate);
        assert!(
            anti_phase_result.integrated_lufs.is_finite(),
            "{rate} Hz anti-phase stereo must not cancel to silence"
        );
        assert!(
            (anti_phase_result.integrated_lufs - single_channel.integrated_lufs - 3.0103).abs() < 0.06,
            "{rate} Hz stereo should sum channel powers: {} vs {}",
            anti_phase_result.integrated_lufs,
            single_channel.integrated_lufs
        );
        assert!(
            (anti_phase_result.true_peak_db - single_channel.true_peak_db).abs() < 0.015,
            "{rate} Hz anti-phase per-channel true peak: {} vs {}",
            anti_phase_result.true_peak_db,
            single_channel.true_peak_db
        );
        let swapped = transform_channels(&anti_phase, true, false, false);
        assert_same_stats(
            measure(&swapped, rate),
            anti_phase_result,
            0.015,
            "anti-phase swap",
        );
    }
}

#[test]
fn adding_an_equal_weight_independent_channel_adds_three_lu_and_peak_uses_the_louder_channel() {
    for rate in RATES {
        let frames = rate as usize * 7;
        let left = tone(rate, 997.0, 0.16, 0.27, frames);
        let right = tone(rate, 997.0, 0.16, 1.03, frames);
        let mono = measure_loudness(&left, 1, rate).expect("valid mono PCM");
        let both = measure(&stereo(&left, &right), rate);
        assert!((both.integrated_lufs - mono.integrated_lufs - 3.0103).abs() < 0.06);

        let quiet = tone(rate, f64::from(rate) / 4.0, 0.04, 0.71, frames);
        let hot = tone(rate, f64::from(rate) / 4.0, 0.31, 0.71, frames);
        let hot_mono = measure_loudness(&hot, 1, rate).expect("valid mono PCM");
        let mixed = stereo(&hot, &quiet);
        let result = measure(&mixed, rate);
        assert!((result.true_peak_db - hot_mono.true_peak_db).abs() < 0.015);
        let exchanged = transform_channels(&mixed, true, false, false);
        assert_same_stats(measure(&exchanged, rate), result, 0.015, "peak channel exchange");
    }
}

#[test]
fn a_changing_envelope_keeps_finite_gated_loudness_and_nonzero_range_under_channel_symmetries() {
    for rate in RATES {
        let frames = rate as usize * 12;
        let left: Vec<f32> = (0..frames)
            .map(|frame| {
                let second = frame / rate as usize;
                let amplitude = match second {
                    0..=2 => 0.20,
                    3..=5 => 0.035,
                    6..=8 => 0.12,
                    _ => 0.055,
                };
                (amplitude * (std::f64::consts::TAU * 997.0 * frame as f64 / f64::from(rate) + 0.2).sin())
                    as f32
            })
            .collect();
        let right = tone(rate, 997.0, 0.09, 1.4, frames);
        let original = stereo(&left, &right);
        let reference = measure(&original, rate);
        assert!(reference.integrated_lufs.is_finite());
        assert!(reference.short_term_lufs.is_finite());
        assert!(
            reference.loudness_range_lu > 0.5,
            "{} LU at {rate}",
            reference.loudness_range_lu
        );
        for transformed in [
            transform_channels(&original, true, false, false),
            transform_channels(&original, false, true, false),
            transform_channels(&original, false, false, true),
        ] {
            assert_same_stats(measure(&transformed, rate), reference, 0.02, "envelope symmetry");
        }
    }
}

#[test]
fn silence_has_no_gated_loudness_and_no_true_peak() {
    for rate in RATES {
        let silence = vec![0.0; rate as usize * 4 * 2];
        let loud = measure(&silence, rate);
        assert_eq!(loud.integrated_lufs, f32::NEG_INFINITY);
        assert_eq!(loud.momentary_lufs, f32::NEG_INFINITY);
        assert_eq!(loud.short_term_lufs, f32::NEG_INFINITY);
        // `gain_to_db` reports exact zero gain at the meter's −100 dB floor.
        assert_eq!(loud.true_peak_db, -100.0);
        assert_eq!(loud.loudness_range_lu, 0.0);
    }
}

#[test]
fn unsupported_surround_layouts_are_rejected_instead_of_assuming_channel_order_or_lfe_policy() {
    // 6 channels are 5.1 since ADR-7227, which fixes the order (L, R, C, LFE, Ls, Rs) and the LFE
    // policy (excluded, BS.1770-4 Table 3); `program_loudness::five_one_channels_carry_the_bs_1770_weights`
    // checks them. Every layout without a defined order is still refused.
    for channels in [3, 4, 5, 7, 8] {
        assert!(
            LoudnessMeter::new(channels, 48_000).is_err(),
            "{channels} channels"
        );
    }
}
