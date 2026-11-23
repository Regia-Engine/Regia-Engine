// SPDX-License-Identifier: MPL-2.0
//! Loudness accuracy across sample rates (ITU-R BS.1770-4): the K-weighting per rate and the
//! Annex 2 true peak, on synthetic signals. `the_true_peak_*` and the `-20 LUFS` reading are the
//! values a calibrated meter must show, not values read back from this one.

use regia_audio::{LoudnessMeter, measure_loudness, true_peak_oversampling};

const RATES: [u32; 3] = [44_100, 48_000, 96_000];

/// A sine of `seconds` at `hz`, peak amplitude `amp`, start phase `phase` radians, mono.
fn sine(rate: u32, hz: f64, seconds: f64, amp: f64, phase: f64) -> Vec<f32> {
    let frames = (seconds * f64::from(rate)) as usize;
    (0..frames)
        .map(|i| {
            let t = i as f64 / f64::from(rate);
            (amp * (std::f64::consts::TAU * hz * t + phase).sin()) as f32
        })
        .collect()
}

/// The same signal on both channels, interleaved.
fn in_phase_stereo(mono: &[f32]) -> Vec<f32> {
    mono.iter().flat_map(|&s| [s, s]).collect()
}

fn db_to_amp(db: f64) -> f64 {
    10f64.powf(db / 20.0)
}

#[test]
fn a_997_hz_sine_at_minus_20_dbfs_reads_minus_20_lufs_at_every_rate() {
    // BS.1770: a 997 Hz sine at 0 dBFS on one channel reads −3.01 LKFS, so two in-phase channels
    // read the level of the sine. This is the stereo calibration signal of EBU Tech 3341.
    for rate in RATES {
        let mono = sine(rate, 997.0, 5.0, db_to_amp(-20.0), 0.0);
        let loud = measure_loudness(&in_phase_stereo(&mono), 2, rate).expect("valid");
        assert!(
            (loud.integrated_lufs + 20.0).abs() < 0.1,
            "{rate} Hz: {}",
            loud.integrated_lufs
        );
    }
}

#[test]
fn one_channel_of_that_sine_reads_3_01_lu_lower_at_every_rate() {
    for rate in RATES {
        let mono = sine(rate, 997.0, 5.0, db_to_amp(-20.0), 0.0);
        let loud = measure_loudness(&mono, 1, rate).expect("valid");
        assert!(
            (loud.integrated_lufs + 23.01).abs() < 0.1,
            "{rate} Hz: {}",
            loud.integrated_lufs
        );
    }
}

#[test]
fn the_reading_of_a_tone_does_not_depend_on_the_rate() {
    // At 96 kHz the old 48 kHz coefficients put the shelf at 3.4 kHz and the high-pass at 76 Hz,
    // so 100 Hz and 997 Hz both read low.
    for hz in [100.0, 500.0, 997.0, 2_000.0, 4_000.0, 8_000.0] {
        let at_rate = |rate: u32| {
            measure_loudness(&sine(rate, hz, 3.0, 0.2, 0.0), 1, rate)
                .expect("valid")
                .integrated_lufs
        };
        let reference = at_rate(48_000);
        for rate in [44_100, 96_000, 192_000] {
            let reading = at_rate(rate);
            assert!(
                (reading - reference).abs() < 0.1,
                "{hz} Hz at {rate} Hz: {reading} vs {reference}"
            );
        }
    }
}

#[test]
fn the_true_peak_of_a_quarter_rate_sine_at_45_degrees_is_the_analytic_peak() {
    // Every sample of sin(π/2·n + π/4) has magnitude amp/√2, 3.01 dB under the peak between them.
    for rate in RATES {
        for amp_db in [0.0f64, -6.0, -20.0] {
            let amp = db_to_amp(amp_db);
            let hz = f64::from(rate) / 4.0;
            let mut samples = sine(rate, hz, 0.5, amp, std::f64::consts::FRAC_PI_4);
            // A sine that starts at full level overshoots its steady peak by about 0.05 dB, and a
            // true-peak meter rightly reads that. A short fade-in measures the steady sine.
            let fade = rate as usize / 100;
            for (i, sample) in samples[..fade].iter_mut().enumerate() {
                let t = i as f64 / fade as f64;
                *sample *= (0.5 - 0.5 * (std::f64::consts::PI * t).cos()) as f32;
            }
            let sample_peak = samples.iter().fold(0.0f32, |peak, s| peak.max(s.abs()));
            let sample_peak_db = 20.0 * f64::from(sample_peak).log10();
            assert!((sample_peak_db - (amp_db - 3.0103)).abs() < 1e-3);
            let reading = f64::from(measure_loudness(&samples, 1, rate).expect("valid").true_peak_db);
            assert!(
                reading > sample_peak_db + 2.8,
                "{rate} Hz, {amp_db} dB: {reading}"
            );
            assert!(
                (reading - amp_db).abs() < 0.1,
                "{rate} Hz, {amp_db} dB: read {reading}"
            );
        }
    }
}

#[test]
fn the_oversampling_factor_follows_annex_2() {
    assert_eq!(true_peak_oversampling(44_100), 4);
    assert_eq!(true_peak_oversampling(48_000), 4);
    assert_eq!(true_peak_oversampling(88_200), 4);
    assert_eq!(true_peak_oversampling(96_000), 2);
    assert_eq!(true_peak_oversampling(192_000), 2);
}

#[test]
fn a_true_peak_never_reads_below_the_sample_peak() {
    // A lone full-scale sample: the filter's tallest tap is 0.972, under the sample itself.
    for rate in RATES {
        let mut samples = vec![0.0f32; 64];
        samples[32] = 1.0;
        let reading = measure_loudness(&samples, 1, rate).expect("valid").true_peak_db;
        assert!(reading >= 0.0, "{rate} Hz: {reading}");
    }
}

#[test]
fn a_clip_shorter_than_the_filter_still_gets_its_inter_sample_peak() {
    let samples = sine(48_000, 12_000.0, 8.0 / 48_000.0, 1.0, std::f64::consts::FRAC_PI_4);
    assert_eq!(samples.len(), 8);
    let reading = measure_loudness(&samples, 1, 48_000).expect("valid").true_peak_db;
    assert!(reading > -3.0103 + 1.0, "{reading}");
}

#[test]
fn stereo_takes_the_peak_of_the_louder_channel() {
    let loud = sine(48_000, 12_000.0, 0.1, 1.0, std::f64::consts::FRAC_PI_4);
    let quiet = sine(48_000, 12_000.0, 0.1, 0.25, std::f64::consts::FRAC_PI_4);
    let interleaved: Vec<f32> = loud.iter().zip(&quiet).flat_map(|(&l, &r)| [l, r]).collect();
    let reading = measure_loudness(&interleaved, 2, 48_000)
        .expect("valid")
        .true_peak_db;
    assert!(reading.abs() < 0.1, "{reading}");
}

#[test]
fn non_finite_samples_do_not_poison_the_filters() {
    let mut meter = LoudnessMeter::new(1, 96_000).expect("valid");
    meter.push(&[f32::NAN, f32::INFINITY, 0.5, -0.5]);
    assert!(meter.true_peak_db().is_finite());
    assert!(meter.momentary_lufs().is_finite());
}
